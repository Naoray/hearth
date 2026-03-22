use std::path::{Path, PathBuf};

use tracing::info;

/// Resolve the Mailpit binary using a resolution chain:
/// 1. Hearth cache: ~/.config/hearth/services/mailpit/mailpit
/// 2. System PATH: `which mailpit`
/// 3. Not found (caller can trigger download)
pub fn resolve_mailpit_binary(config_dir: &Path) -> Option<PathBuf> {
    // 1. Hearth cache
    let cached = config_dir.join("services/mailpit/mailpit");
    if cached.exists() {
        info!(path = %cached.display(), "resolved Mailpit from Hearth cache");
        return Some(cached);
    }

    // 2. System PATH
    if let Ok(output) = std::process::Command::new("which")
        .arg("mailpit")
        .output()
    {
        let succeeded = output.status.success();
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if succeeded && path.exists() {
            info!(path = %path.display(), "resolved Mailpit from system PATH");
            return Some(path);
        }
    }

    // 3. Not found
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_from_hearth_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_path = tmp.path().join("services/mailpit/mailpit");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "fake-binary").unwrap();

        let result = resolve_mailpit_binary(tmp.path());
        assert_eq!(result, Some(bin_path));
    }

    #[test]
    fn returns_none_when_not_installed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = resolve_mailpit_binary(tmp.path());
        // May find system mailpit, so we check it doesn't point to our temp dir
        if let Some(path) = &result {
            assert!(!path.starts_with(tmp.path()));
        }
    }

    #[test]
    fn hearth_cache_takes_priority() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_path = tmp.path().join("services/mailpit/mailpit");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "hearth-version").unwrap();

        let result = resolve_mailpit_binary(tmp.path());
        assert!(result.is_some());
        assert!(result.unwrap().starts_with(tmp.path()));
    }
}
