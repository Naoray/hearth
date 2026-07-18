//! Resolve the Laravel site that `hearth add` should operate on.
//!
//! Strategy: `--site=PATH` wins; else walk up from cwd until an ancestor matches a
//! linked `Site.path`. Canonicalizes both sides so symlinked workspaces aren't
//! misreported as "not in a linked site" (see dissent H5 in scratchpad 796).

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::info;

use crate::add::laravel;
use crate::config::HearthConfig;
use crate::php::resolver::resolve_php_binary;
use crate::site::{Site, SiteManager};

#[derive(Debug, Error)]
pub enum SiteResolutionError {
    #[error(
        "not in a linked Hearth site (cwd: {cwd}); run `hearth link` or pass --site=PATH"
    )]
    NotInLinkedSite { cwd: PathBuf },

    #[error("site path {0} is not in the linked sites list")]
    SiteNotLinked(PathBuf),

    #[error(
        "site {0} has no resolved path on disk; re-run `hearth link` from inside the project"
    )]
    SiteLinkedButPathUnresolved(String),

    #[error(
        "{0} does not look like a Laravel app (missing composer.json or laravel/framework)"
    )]
    NotALaravelApp(PathBuf),

    #[error("could not resolve PHP {version} binary")]
    PhpUnavailable { version: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone)]
pub struct SiteContext {
    pub site: Site,
    pub php_binary: PathBuf,
    /// Version `php_binary` was resolved for (site isolation or default_php).
    /// Threaded into workers so their scan-dir env can be reconstructed later.
    pub php_version: String,
    pub laravel_constraint: String,
}

/// Resolve the target site, or error.
///
/// `cwd` is canonicalized to handle symlinked workspaces. Empty-path `Site` entries
/// (Herd-Nginx-only sites with unresolvable symlinks) are filtered before matching.
pub fn resolve(
    explicit: Option<&Path>,
    cwd: &Path,
    site_manager: &SiteManager,
    config: &HearthConfig,
    config_dir: &Path,
) -> Result<SiteContext, SiteResolutionError> {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    let sites: Vec<Site> = site_manager
        .list_sites()
        .map_err(SiteResolutionError::Other)?
        .into_iter()
        .filter(|s| !s.path.as_os_str().is_empty())
        .collect();

    let site = match explicit {
        Some(path) => resolve_explicit(path, &sites)?,
        None => walk_up_for_site(&canonical_cwd, &sites)?,
    };

    if site.path.as_os_str().is_empty() {
        return Err(SiteResolutionError::SiteLinkedButPathUnresolved(
            site.name.clone(),
        ));
    }

    let composer_json = site.path.join("composer.json");
    if !composer_json.exists() {
        return Err(SiteResolutionError::NotALaravelApp(site.path.clone()));
    }
    let laravel_constraint = laravel::framework_constraint(&site.path)
        .map_err(|_| SiteResolutionError::NotALaravelApp(site.path.clone()))?;

    let version = site
        .php_version
        .clone()
        .unwrap_or_else(|| config.default_php.clone());
    let php_binary = resolve_php_binary(&version, &config_dir.to_path_buf())
        .ok_or(SiteResolutionError::PhpUnavailable {
            version: version.clone(),
        })?;
    info!(site = %site.name, php = %php_binary.display(), "resolved site context");

    Ok(SiteContext {
        site,
        php_binary,
        php_version: version,
        laravel_constraint,
    })
}

fn resolve_explicit(path: &Path, sites: &[Site]) -> Result<Site, SiteResolutionError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| SiteResolutionError::SiteNotLinked(path.to_path_buf()))?;
    // If the path matches a linked site, reuse its Site (preserves SSL +
    // per-site PHP isolation).
    if let Some(s) = sites.iter().find(|s| {
        std::fs::canonicalize(&s.path)
            .map(|p| p == canonical)
            .unwrap_or(false)
    }) {
        return Ok(s.clone());
    }
    // Otherwise synthesize a Site from the path. `hearth add` only needs the
    // path + a display name; linking (Valet/Herd nginx) is orthogonal and
    // requires sudo. The Laravel-framework check happens after this point.
    let name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("site")
        .to_string();
    Ok(Site {
        name,
        path: canonical,
        secured: false,
        php_version: None,
    })
}

fn walk_up_for_site(cwd: &Path, sites: &[Site]) -> Result<Site, SiteResolutionError> {
    let mut current: Option<&Path> = Some(cwd);
    while let Some(dir) = current {
        for s in sites {
            if let Ok(canon_site) = std::fs::canonicalize(&s.path)
                && canon_site == dir
            {
                return Ok(s.clone());
            }
        }
        current = dir.parent();
    }
    Err(SiteResolutionError::NotInLinkedSite {
        cwd: cwd.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_laravel_skeleton(dir: &Path, laravel_version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("composer.json"),
            format!(
                r#"{{"require":{{"laravel/framework":"{laravel_version}"}}}}"#
            ),
        )
        .unwrap();
    }

    fn fake_php(config_dir: &Path, version: &str) -> PathBuf {
        let php_dir = config_dir.join("php").join(version);
        std::fs::create_dir_all(&php_dir).unwrap();
        let bin = php_dir.join("php");
        std::fs::write(&bin, "").unwrap();
        bin
    }

    fn linked_site(valet_home: &Path, name: &str, target: &Path) {
        let sites_dir = valet_home.join("Sites");
        std::fs::create_dir_all(&sites_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, sites_dir.join(name)).unwrap();
    }

    #[test]
    fn resolves_via_explicit_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site_root = tmp.path().join("app");
        write_laravel_skeleton(&site_root, "^11.0");

        let valet = tmp.path().join("valet");
        linked_site(&valet, "app", &site_root);

        let config_dir = tmp.path().join("config");
        fake_php(&config_dir, "8.4");

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default();
        let ctx = resolve(Some(&site_root), tmp.path(), &mgr, &config, &config_dir).unwrap();
        assert_eq!(ctx.site.name, "app");
        assert_eq!(ctx.laravel_constraint, "^11.0");
    }

    #[test]
    fn resolves_via_cwd_when_inside_site() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site_root = tmp.path().join("app");
        write_laravel_skeleton(&site_root, "^10.0");

        let valet = tmp.path().join("valet");
        linked_site(&valet, "app", &site_root);

        let config_dir = tmp.path().join("config");
        fake_php(&config_dir, "8.4");

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default();
        let ctx = resolve(None, &site_root, &mgr, &config, &config_dir).unwrap();
        assert_eq!(ctx.site.name, "app");
    }

    #[test]
    fn walks_up_to_find_ancestor_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site_root = tmp.path().join("app");
        write_laravel_skeleton(&site_root, "^11.0");
        let nested = site_root.join("app/Http/Controllers");
        std::fs::create_dir_all(&nested).unwrap();

        let valet = tmp.path().join("valet");
        linked_site(&valet, "app", &site_root);

        let config_dir = tmp.path().join("config");
        fake_php(&config_dir, "8.4");

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default();
        let ctx = resolve(None, &nested, &mgr, &config, &config_dir).unwrap();
        assert_eq!(ctx.site.name, "app");
    }

    #[test]
    fn errors_when_not_in_any_site() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("not-a-site");
        std::fs::create_dir_all(&outside).unwrap();

        let valet = tmp.path().join("valet");
        std::fs::create_dir_all(&valet).unwrap();

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default();
        let config_dir = tmp.path().join("config");
        let err = resolve(None, &outside, &mgr, &config, &config_dir).unwrap_err();
        assert!(matches!(err, SiteResolutionError::NotInLinkedSite { .. }));
    }

    #[test]
    fn accepts_explicit_unlinked_path_when_laravel_app() {
        // `--site=<path>` should accept any directory that LOOKS like a
        // Laravel app, even if not linked in Valet/Herd. Linking is for
        // serving (sudo-gated); add-recipes only need composer + .env.
        let tmp = tempfile::TempDir::new().unwrap();
        let site_root = tmp.path().join("unlinked");
        std::fs::create_dir_all(&site_root).unwrap();
        write_laravel_skeleton(&site_root, "^11.0");

        let valet = tmp.path().join("valet");
        std::fs::create_dir_all(&valet).unwrap();

        let config_dir = tmp.path().join("config");
        fake_php(&config_dir, "8.4");

        let mgr = SiteManager::new(valet, "test".to_string());
        let mut config = HearthConfig::default();
        config.default_php = "8.4".to_string();

        let ctx = resolve(Some(&site_root), tmp.path(), &mgr, &config, &config_dir).unwrap();
        assert_eq!(ctx.site.name, "unlinked");
        assert_eq!(ctx.site.path, std::fs::canonicalize(&site_root).unwrap());
        assert!(!ctx.site.secured);
    }

    #[test]
    fn errors_when_explicit_path_not_a_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("nope");

        let valet = tmp.path().join("valet");
        std::fs::create_dir_all(&valet).unwrap();

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default();
        let config_dir = tmp.path().join("config");
        let err = resolve(Some(&missing), tmp.path(), &mgr, &config, &config_dir).unwrap_err();
        assert!(matches!(err, SiteResolutionError::SiteNotLinked(_)));
    }

    #[test]
    fn errors_when_no_composer_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site_root = tmp.path().join("not-laravel");
        std::fs::create_dir_all(&site_root).unwrap();

        let valet = tmp.path().join("valet");
        linked_site(&valet, "not-laravel", &site_root);

        let config_dir = tmp.path().join("config");
        fake_php(&config_dir, "8.4");

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default();
        let err = resolve(Some(&site_root), tmp.path(), &mgr, &config, &config_dir).unwrap_err();
        assert!(matches!(err, SiteResolutionError::NotALaravelApp(_)));
    }

    #[test]
    fn errors_when_composer_missing_laravel_framework() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site_root = tmp.path().join("php-app");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(
            site_root.join("composer.json"),
            r#"{"require":{"symfony/console":"^7.0"}}"#,
        )
        .unwrap();

        let valet = tmp.path().join("valet");
        linked_site(&valet, "php-app", &site_root);

        let config_dir = tmp.path().join("config");
        fake_php(&config_dir, "8.4");

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default();
        let err = resolve(Some(&site_root), tmp.path(), &mgr, &config, &config_dir).unwrap_err();
        assert!(matches!(err, SiteResolutionError::NotALaravelApp(_)));
    }

    #[test]
    fn picks_php_binary_from_valet_isolation_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site_root = tmp.path().join("app");
        write_laravel_skeleton(&site_root, "^10.0");

        // valet isolation file specifies PHP 8.2
        let valet = tmp.path().join("valet");
        linked_site(&valet, "app", &site_root);
        let iso = valet.join("Isolate");
        std::fs::create_dir_all(&iso).unwrap();
        std::fs::write(iso.join("app.test"), "8.2\n").unwrap();

        // both 8.2 and the default 8.4 exist in the config dir
        let config_dir = tmp.path().join("config");
        let bin_82 = fake_php(&config_dir, "8.2");
        let _bin_84 = fake_php(&config_dir, "8.4");

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default(); // default_php = "8.4"
        let ctx = resolve(Some(&site_root), tmp.path(), &mgr, &config, &config_dir).unwrap();
        assert_eq!(ctx.php_binary, bin_82);
    }

    #[test]
    fn errors_when_php_binary_unavailable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site_root = tmp.path().join("app");
        write_laravel_skeleton(&site_root, "^11.0");

        let valet = tmp.path().join("valet");
        linked_site(&valet, "app", &site_root);
        let iso = valet.join("Isolate");
        std::fs::create_dir_all(&iso).unwrap();
        // Use a version that almost certainly is not on the host
        std::fs::write(iso.join("app.test"), "5.5\n").unwrap();

        let mgr = SiteManager::new(valet, "test".to_string());
        let config = HearthConfig::default();
        let config_dir = tmp.path().join("nonexistent-config");
        let err = resolve(Some(&site_root), tmp.path(), &mgr, &config, &config_dir).unwrap_err();
        assert!(matches!(err, SiteResolutionError::PhpUnavailable { .. }));
    }
}
