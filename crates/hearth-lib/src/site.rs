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
                php_version: None, // TODO: read from Valet isolation config
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
}
