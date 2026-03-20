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

/// Site management — delegates to Valet CLI for actual operations.
///
/// Hearth wraps Valet commands and adds:
/// - Lazy site enumeration (for GUI performance)
/// - PHP version tracking per-site
/// - Integration with Anvil worktrees
pub struct SiteManager {
    valet_home: PathBuf,
}

impl SiteManager {
    pub fn new(valet_home: PathBuf) -> Self {
        Self { valet_home }
    }

    /// List all linked sites by reading Valet's Nginx config directory.
    /// Uses lazy enumeration — only reads directory entries, not file contents.
    pub fn list_sites(&self) -> anyhow::Result<Vec<Site>> {
        let nginx_dir = self.valet_home.join("Nginx");
        if !nginx_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sites = Vec::new();
        for entry in std::fs::read_dir(&nginx_dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_string_lossy()
                .to_string();

            // Skip hidden files
            if name.starts_with('.') {
                continue;
            }

            let cert_path = self
                .valet_home
                .join("Certificates")
                .join(format!("{}.crt", name));

            sites.push(Site {
                name: name.clone(),
                path: self.resolve_site_path(&name).unwrap_or_default(),
                secured: cert_path.exists(),
                php_version: self.resolve_isolated_php(&name),
            });
        }

        sites.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sites)
    }

    /// Resolve a site name to its filesystem path using Valet's link symlinks.
    fn resolve_site_path(&self, site_name: &str) -> Option<PathBuf> {
        let link_path = self.valet_home.join("Sites").join(site_name);
        if link_path.is_symlink() {
            std::fs::read_link(&link_path).ok()
        } else {
            None
        }
    }

    /// Get the Valet home path (for testing).
    #[cfg(test)]
    pub fn valet_home(&self) -> &std::path::Path {
        &self.valet_home
    }

    /// Read the isolated PHP version for a site from Valet's Isolate directory.
    ///
    /// Valet stores isolation as files named `{site_name}{tld}` containing the
    /// PHP version string (e.g., "8.3").
    fn resolve_isolated_php(&self, site_name: &str) -> Option<String> {
        let isolate_dir = self.valet_home.join("Isolate");
        if !isolate_dir.exists() {
            return None;
        }

        // Valet names isolation files as "{site}.test" or just "{site}"
        for suffix in &[".test", ""] {
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
        // Create Nginx config directory with site entries
        let nginx_dir = tmp.join("Nginx");
        std::fs::create_dir_all(&nginx_dir).unwrap();
        std::fs::write(nginx_dir.join("alpha"), "").unwrap();
        std::fs::write(nginx_dir.join("beta"), "").unwrap();
        std::fs::write(nginx_dir.join(".hidden"), "").unwrap();

        // Create a symlink for alpha site path
        let sites_dir = tmp.join("Sites");
        std::fs::create_dir_all(&sites_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp/alpha-project", sites_dir.join("alpha")).unwrap();

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

        let manager = SiteManager::new(tmp.path().to_path_buf());
        let sites = manager.list_sites().unwrap();

        assert_eq!(sites.len(), 2); // .hidden should be excluded
        assert_eq!(sites[0].name, "alpha");
        assert_eq!(sites[1].name, "beta");
    }

    #[test]
    fn list_sites_detects_secured() {
        let tmp = tempfile::TempDir::new().unwrap();
        setup_mock_valet(tmp.path());

        let manager = SiteManager::new(tmp.path().to_path_buf());
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

        let manager = SiteManager::new(tmp.path().to_path_buf());
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

        let manager = SiteManager::new(tmp.path().to_path_buf());
        let sites = manager.list_sites().unwrap();

        let alpha = sites.iter().find(|s| s.name == "alpha").unwrap();
        assert_eq!(alpha.path, PathBuf::from("/tmp/alpha-project"));
    }

    #[test]
    fn list_sites_empty_when_no_nginx_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manager = SiteManager::new(tmp.path().to_path_buf());
        let sites = manager.list_sites().unwrap();
        assert!(sites.is_empty());
    }
}
