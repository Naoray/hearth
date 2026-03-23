use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Represents a linked site managed by Valet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    pub name: String,
    pub path: PathBuf,
    pub secured: bool,
    pub php_version: Option<String>,
}

/// Site management — reads sites from multiple valet home directories.
///
/// Merges sites from Valet's config (`~/.config/valet`) and Herd's config
/// (`~/Library/Application Support/Herd/config/valet`). Sites from earlier
/// paths take priority when names collide.
pub struct SiteManager {
    valet_homes: Vec<PathBuf>,
    tld: String,
}

impl SiteManager {
    pub fn new(valet_home: PathBuf, tld: String) -> Self {
        Self {
            valet_homes: vec![valet_home],
            tld,
        }
    }

    /// Create a SiteManager that reads from multiple valet home directories.
    pub fn with_homes(valet_homes: Vec<PathBuf>, tld: String) -> Self {
        Self { valet_homes, tld }
    }

    /// List all linked sites across all valet home directories.
    ///
    /// For each home, reads `Sites/` (symlinks) and `Nginx/` (config files).
    /// Earlier homes take priority — duplicates from later homes are skipped.
    pub fn list_sites(&self) -> anyhow::Result<Vec<Site>> {
        let mut seen = std::collections::HashSet::new();
        let mut sites = Vec::new();

        for valet_home in &self.valet_homes {
            // Read Sites/ directory (symlinks from `valet link`)
            let sites_dir = valet_home.join("Sites");
            if sites_dir.exists() {
                for entry in std::fs::read_dir(&sites_dir)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') || seen.contains(&name) {
                        continue;
                    }

                    let path = if entry.path().is_symlink() {
                        std::fs::read_link(entry.path()).unwrap_or_default()
                    } else {
                        entry.path()
                    };

                    let cert_path = valet_home
                        .join("Certificates")
                        .join(format!("{}.crt", name));

                    sites.push(Site {
                        name: name.clone(),
                        path,
                        secured: cert_path.exists(),
                        php_version: self.resolve_isolated_php_in(valet_home, &name),
                    });
                    seen.insert(name);
                }
            }

            // Read Nginx/ directory (config files, used by Herd)
            let nginx_dir = valet_home.join("Nginx");
            if nginx_dir.exists() {
                for entry in std::fs::read_dir(&nginx_dir)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') || seen.contains(&name) {
                        continue;
                    }

                    // Strip .test suffix from Herd Nginx config names
                    let site_name = name
                        .strip_suffix(&format!(".{}", self.tld))
                        .unwrap_or(&name)
                        .to_string();

                    if seen.contains(&site_name) {
                        continue;
                    }

                    let path = self.resolve_site_path_in(valet_home, &site_name)
                        .unwrap_or_default();

                    let cert_path = valet_home
                        .join("Certificates")
                        .join(format!("{}.crt", site_name));

                    sites.push(Site {
                        name: site_name.clone(),
                        path,
                        secured: cert_path.exists(),
                        php_version: self.resolve_isolated_php_in(valet_home, &site_name),
                    });
                    seen.insert(site_name);
                }
            }
        }

        sites.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sites)
    }

    /// Resolve a site name to its filesystem path in a specific valet home.
    fn resolve_site_path_in(&self, valet_home: &std::path::Path, site_name: &str) -> Option<PathBuf> {
        let link_path = valet_home.join("Sites").join(site_name);
        if link_path.is_symlink() {
            std::fs::read_link(&link_path).ok()
        } else {
            None
        }
    }

    /// Read the isolated PHP version for a site in a specific valet home.
    fn resolve_isolated_php_in(&self, valet_home: &std::path::Path, site_name: &str) -> Option<String> {
        let isolate_dir = valet_home.join("Isolate");
        if !isolate_dir.exists() {
            return None;
        }

        // Valet names isolation files as "{site}.{tld}" or just "{site}"
        let tld_suffix = format!(".{}", self.tld);
        for suffix in &[tld_suffix.as_str(), ""] {
            let iso_file = isolate_dir.join(format!("{site_name}{suffix}"));
            if iso_file.exists()
                && let Ok(content) = std::fs::read_to_string(&iso_file)
            {
                let version = content.trim().to_string();
                if !version.is_empty() {
                    return Some(version);
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_mock_valet(tmp: &std::path::Path) {
        // Create Sites directory with symlinks (primary source)
        let sites_dir = tmp.join("Sites");
        std::fs::create_dir_all(&sites_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp/alpha-project", sites_dir.join("alpha")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp/beta-project", sites_dir.join("beta")).unwrap();

        // Hidden entry in Sites should be skipped
        std::fs::write(sites_dir.join(".hidden"), "").unwrap();

        // Create certificate for beta (secured)
        let certs_dir = tmp.join("Certificates");
        std::fs::create_dir_all(&certs_dir).unwrap();
        std::fs::write(certs_dir.join("beta.crt"), "fake-cert").unwrap();

        // Create isolation config for alpha
        let isolate_dir = tmp.join("Isolate");
        std::fs::create_dir_all(&isolate_dir).unwrap();
        std::fs::write(isolate_dir.join("alpha.test"), "8.3\n").unwrap();
    }

    #[test]
    fn list_sites_returns_sorted_sites() {
        let tmp = tempfile::TempDir::new().unwrap();
        setup_mock_valet(tmp.path());

        let manager = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites = manager.list_sites().unwrap();

        assert_eq!(sites.len(), 2); // .hidden should be excluded
        assert_eq!(sites[0].name, "alpha");
        assert_eq!(sites[1].name, "beta");
    }

    #[test]
    fn list_sites_detects_secured() {
        let tmp = tempfile::TempDir::new().unwrap();
        setup_mock_valet(tmp.path());

        let manager = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites = manager.list_sites().unwrap();

        let alpha = sites.iter().find(|s| s.name == "alpha").unwrap();
        let beta = sites.iter().find(|s| s.name == "beta").unwrap();
        assert!(!alpha.secured);
        assert!(beta.secured);
    }

    #[test]
    fn list_sites_reads_php_isolation() {
        let tmp = tempfile::TempDir::new().unwrap();
        setup_mock_valet(tmp.path());

        let manager = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites = manager.list_sites().unwrap();

        let alpha = sites.iter().find(|s| s.name == "alpha").unwrap();
        let beta = sites.iter().find(|s| s.name == "beta").unwrap();
        assert_eq!(alpha.php_version.as_deref(), Some("8.3"));
        assert_eq!(beta.php_version, None);
    }

    #[cfg(unix)]
    #[test]
    fn list_sites_resolves_symlink_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        setup_mock_valet(tmp.path());

        let manager = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites = manager.list_sites().unwrap();

        let alpha = sites.iter().find(|s| s.name == "alpha").unwrap();
        assert_eq!(alpha.path, PathBuf::from("/tmp/alpha-project"));
    }

    #[test]
    fn list_sites_empty_when_no_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manager = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites = manager.list_sites().unwrap();
        assert!(sites.is_empty());
    }

    #[test]
    fn list_sites_falls_back_to_nginx_dir() {
        let tmp = tempfile::TempDir::new().unwrap();

        // No Sites/ dir, only Nginx/ (Herd-managed)
        let nginx_dir = tmp.path().join("Nginx");
        std::fs::create_dir_all(&nginx_dir).unwrap();
        std::fs::write(nginx_dir.join("herd-site"), "").unwrap();

        let manager = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites = manager.list_sites().unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].name, "herd-site");
    }

    #[test]
    fn list_sites_deduplicates_across_sources() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Same site in both Sites/ and Nginx/
        let sites_dir = tmp.path().join("Sites");
        std::fs::create_dir_all(&sites_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp/mysite", sites_dir.join("mysite")).unwrap();

        let nginx_dir = tmp.path().join("Nginx");
        std::fs::create_dir_all(&nginx_dir).unwrap();
        std::fs::write(nginx_dir.join("mysite"), "").unwrap();

        let manager = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites = manager.list_sites().unwrap();
        assert_eq!(sites.len(), 1); // no duplicate
        assert_eq!(sites[0].name, "mysite");
    }

    #[test]
    fn isolation_uses_configured_tld() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Setup site via Sites/ dir
        let sites_dir = tmp.path().join("Sites");
        std::fs::create_dir_all(&sites_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp/mysite", sites_dir.join("mysite")).unwrap();

        // Isolation file uses custom TLD ".dev"
        let isolate_dir = tmp.path().join("Isolate");
        std::fs::create_dir_all(&isolate_dir).unwrap();
        std::fs::write(isolate_dir.join("mysite.dev"), "8.2\n").unwrap();

        // With matching TLD: should find isolation
        let manager = SiteManager::new(tmp.path().to_path_buf(), "dev".to_string());
        let sites = manager.list_sites().unwrap();
        let site = sites.iter().find(|s| s.name == "mysite").unwrap();
        assert_eq!(site.php_version.as_deref(), Some("8.2"));

        // With non-matching TLD: should NOT find isolation
        let manager2 = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites2 = manager2.list_sites().unwrap();
        let site2 = sites2.iter().find(|s| s.name == "mysite").unwrap();
        assert_eq!(site2.php_version, None);
    }

    #[test]
    fn isolation_fallback_to_bare_filename() {
        let tmp = tempfile::TempDir::new().unwrap();

        let sites_dir = tmp.path().join("Sites");
        std::fs::create_dir_all(&sites_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp/legacy", sites_dir.join("legacy")).unwrap();

        // Isolation file without TLD suffix (bare name)
        let isolate_dir = tmp.path().join("Isolate");
        std::fs::create_dir_all(&isolate_dir).unwrap();
        std::fs::write(isolate_dir.join("legacy"), "7.4\n").unwrap();

        let manager = SiteManager::new(tmp.path().to_path_buf(), "test".to_string());
        let sites = manager.list_sites().unwrap();
        let site = sites.iter().find(|s| s.name == "legacy").unwrap();
        assert_eq!(site.php_version.as_deref(), Some("7.4"));
    }

    #[cfg(unix)]
    #[test]
    fn with_homes_merges_sites_from_multiple_dirs() {
        let valet_dir = tempfile::TempDir::new().unwrap();
        let herd_dir = tempfile::TempDir::new().unwrap();

        // Valet has site "alpha"
        let valet_sites = valet_dir.path().join("Sites");
        std::fs::create_dir_all(&valet_sites).unwrap();
        std::os::unix::fs::symlink("/tmp/alpha", valet_sites.join("alpha")).unwrap();

        // Herd has site "beta" (via Nginx config)
        let herd_nginx = herd_dir.path().join("Nginx");
        std::fs::create_dir_all(&herd_nginx).unwrap();
        std::fs::write(herd_nginx.join("beta.test"), "").unwrap();

        // Herd also has "beta" in Sites
        let herd_sites = herd_dir.path().join("Sites");
        std::fs::create_dir_all(&herd_sites).unwrap();
        std::os::unix::fs::symlink("/tmp/beta", herd_sites.join("beta")).unwrap();

        let manager = SiteManager::with_homes(
            vec![valet_dir.path().to_path_buf(), herd_dir.path().to_path_buf()],
            "test".to_string(),
        );
        let sites = manager.list_sites().unwrap();
        let names: Vec<&str> = sites.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "should contain alpha: {names:?}");
        assert!(names.contains(&"beta"), "should contain beta: {names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn with_homes_first_home_takes_priority() {
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();

        // Same site in both — first should win
        let first_sites = first.path().join("Sites");
        std::fs::create_dir_all(&first_sites).unwrap();
        std::os::unix::fs::symlink("/tmp/first-path", first_sites.join("mysite")).unwrap();

        let second_sites = second.path().join("Sites");
        std::fs::create_dir_all(&second_sites).unwrap();
        std::os::unix::fs::symlink("/tmp/second-path", second_sites.join("mysite")).unwrap();

        let manager = SiteManager::with_homes(
            vec![first.path().to_path_buf(), second.path().to_path_buf()],
            "test".to_string(),
        );
        let sites = manager.list_sites().unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].path, PathBuf::from("/tmp/first-path"));
    }
}
