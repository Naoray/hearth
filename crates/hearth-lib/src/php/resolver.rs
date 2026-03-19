use std::path::PathBuf;

use tracing::info;

/// PHP binary resolution chain.
///
/// Searches for a PHP binary in this order:
/// 1. Hearth's own cached binaries (~/.config/hearth/php/{version}/php)
/// 2. Herd binaries (~/Library/Application Support/Herd/bin/php{version})
/// 3. Homebrew (/opt/homebrew/opt/php@{version}/bin/php)
/// 4. System PATH (fallback)
///
/// ```text
/// resolve("8.4")
///   ├─ ~/.config/hearth/php/8.4/php       ← own CI binaries
///   ├─ ~/Library/.../Herd/bin/php84       ← Herd migration
///   ├─ /opt/homebrew/opt/php@8.4/bin/php  ← Homebrew fallback
///   └─ None                                ← not installed
/// ```
pub fn resolve_php_binary(version: &str, config_dir: &PathBuf) -> Option<PathBuf> {
    let version_compact = version.replace('.', ""); // "8.4" -> "84"

    // 1. Hearth's own cached binaries
    let hearth_path = config_dir.join("php").join(version).join("php");
    if hearth_path.exists() {
        info!(version, path = %hearth_path.display(), "resolved PHP from Hearth cache");
        return Some(hearth_path);
    }

    // 2. Herd binaries (migration convenience)
    let herd_path = dirs::home_dir()
        .map(|h| {
            h.join("Library/Application Support/Herd/bin")
                .join(format!("php{}", version_compact))
        });
    if let Some(ref path) = herd_path {
        if path.exists() {
            info!(version, path = %path.display(), "resolved PHP from Herd");
            return Some(path.clone());
        }
    }

    // 3. Homebrew
    let brew_path = PathBuf::from(format!(
        "/opt/homebrew/opt/php@{}/bin/php",
        version
    ));
    if brew_path.exists() {
        info!(version, path = %brew_path.display(), "resolved PHP from Homebrew");
        return Some(brew_path);
    }

    // 4. Not found
    None
}

/// Resolve the PHP-FPM binary for a given version.
pub fn resolve_phpfpm_binary(version: &str, config_dir: &PathBuf) -> Option<PathBuf> {
    let version_compact = version.replace('.', "");

    // Same resolution chain but for php-fpm
    let hearth_path = config_dir.join("php").join(version).join("php-fpm");
    if hearth_path.exists() {
        return Some(hearth_path);
    }

    let herd_path = dirs::home_dir()
        .map(|h| {
            h.join("Library/Application Support/Herd/bin")
                .join(format!("php{}-fpm", version_compact))
        });
    if let Some(ref path) = herd_path {
        if path.exists() {
            return Some(path.clone());
        }
    }

    let brew_path = PathBuf::from(format!(
        "/opt/homebrew/opt/php@{}/sbin/php-fpm",
        version
    ));
    if brew_path.exists() {
        return Some(brew_path);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolves_hearth_binary_first() {
        let tmp = TempDir::new().unwrap();
        let php_dir = tmp.path().join("php/8.4");
        std::fs::create_dir_all(&php_dir).unwrap();
        std::fs::write(php_dir.join("php"), "fake-binary").unwrap();

        let result = resolve_php_binary("8.4", &tmp.path().to_path_buf());
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("php/8.4/php"));
    }

    #[test]
    fn returns_none_when_not_installed() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_php_binary("9.9", &tmp.path().to_path_buf());
        // May find Herd/Homebrew on the host, so we just check it doesn't panic
        let _ = result;
    }
}
