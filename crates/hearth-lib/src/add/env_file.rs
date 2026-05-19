//! `.env` file patcher used by recipes to update environment keys.
//!
//! Preserves comments, blank lines, and existing key order. Writes one backup per
//! `patch()` call with filename `.env.hearth.YYYYMMDD-HHMMSS-NNNNNNNNN-PID.bak`
//! — nanos + PID component avoids collisions when scripted runs land in the same
//! second (see dissent B4 in `solo://proj/22/scratchpad/adversarial-review-h--796`).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use tracing::info;

const HEARTH_COMMENT_PREFIX: &str = "# Added by hearth add";

#[derive(Debug, Clone)]
pub struct EnvPatch {
    pub key: String,
    pub value: String,
    /// If true and the key already exists, leave the existing value untouched.
    /// Used for "set only if unset" semantics like Horizon's `REDIS_CLIENT`.
    pub only_if_missing: bool,
}

impl EnvPatch {
    pub fn set(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            only_if_missing: false,
        }
    }

    pub fn set_if_missing(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            only_if_missing: true,
        }
    }
}

#[derive(Debug)]
pub struct PatchResult {
    pub backup_path: Option<PathBuf>,
    pub keys_written: Vec<String>,
}

/// Patch `.env` at `env_path`. Writes a backup before mutating.
pub fn patch(env_path: &Path, patches: &[EnvPatch], comment_tag: &str) -> Result<PatchResult> {
    if patches.is_empty() {
        return Ok(PatchResult {
            backup_path: None,
            keys_written: Vec::new(),
        });
    }
    let existed = env_path.exists();
    let backup_path = if existed { Some(write_backup(env_path)?) } else { None };

    let existing = if existed {
        std::fs::read_to_string(env_path).context("reading .env")?
    } else {
        String::new()
    };

    let (new_content, keys_written) = apply_patches(&existing, patches, comment_tag);
    if let Some(parent) = env_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(env_path, new_content).context("writing .env")?;
    info!(path = %env_path.display(), keys = ?keys_written, "patched .env");
    Ok(PatchResult {
        backup_path,
        keys_written,
    })
}

fn write_backup(env_path: &Path) -> Result<PathBuf> {
    let now = chrono::Utc::now();
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let backup_name = format!(
        ".env.hearth.{}-{:09}-{}.bak",
        now.format("%Y%m%d-%H%M%S"),
        nanos,
        pid
    );
    let parent = env_path.parent().unwrap_or_else(|| Path::new("."));
    let backup_path = parent.join(backup_name);
    std::fs::copy(env_path, &backup_path).context("writing .env backup")?;
    Ok(backup_path)
}

/// Pure function — apply patches to an env file string. Returns (new content, keys that
/// actually got written or replaced).
pub fn apply_patches(
    input: &str,
    patches: &[EnvPatch],
    comment_tag: &str,
) -> (String, Vec<String>) {
    let trailing_newline = input.ends_with('\n');
    let mut lines: Vec<String> = if input.is_empty() {
        Vec::new()
    } else {
        input.split('\n').map(|l| l.to_string()).collect()
    };
    // Splitting an "a\nb\n" string yields ["a","b",""] — drop the trailing empty.
    if trailing_newline {
        lines.pop();
    }

    let mut keys_written: Vec<String> = Vec::new();
    let mut appended: Vec<(String, String)> = Vec::new();

    for patch in patches {
        let mut found = false;
        for line in lines.iter_mut() {
            if let Some(existing_key) = line_key(line)
                && existing_key == patch.key
            {
                if patch.only_if_missing {
                    found = true;
                    break;
                }
                *line = format!("{}={}", patch.key, patch.value);
                keys_written.push(patch.key.clone());
                found = true;
                break;
            }
        }
        if !found {
            // only_if_missing still appends — "if missing" means: write only when the
            // key is absent. Found+only_if_missing leaves the existing value alone.
            appended.push((patch.key.clone(), patch.value.clone()));
            keys_written.push(patch.key.clone());
        }
    }

    let mut out = lines.join("\n");
    if !appended.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("\n{} {}\n", HEARTH_COMMENT_PREFIX, comment_tag));
        for (k, v) in appended {
            out.push_str(&format!("{}={}\n", k, v));
        }
    } else if trailing_newline && !out.ends_with('\n') {
        out.push('\n');
    }
    (out, keys_written)
}

fn line_key(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let eq_pos = trimmed.find('=')?;
    let key = trimmed[..eq_pos].trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kv_lines() {
        assert_eq!(line_key("APP_ENV=local"), Some("APP_ENV".to_string()));
        assert_eq!(line_key("  APP_KEY = base64:..."), Some("APP_KEY".to_string()));
    }

    #[test]
    fn ignores_comments_and_blanks() {
        assert!(line_key("").is_none());
        assert!(line_key("# comment").is_none());
        assert!(line_key("   ").is_none());
    }

    #[test]
    fn replaces_existing_key_in_place() {
        let input = "APP_ENV=local\nAPP_DEBUG=true\nQUEUE_CONNECTION=sync\n";
        let (out, written) =
            apply_patches(input, &[EnvPatch::set("QUEUE_CONNECTION", "redis")], "horizon");
        assert_eq!(written, vec!["QUEUE_CONNECTION"]);
        assert!(out.contains("QUEUE_CONNECTION=redis"));
        assert!(!out.contains("QUEUE_CONNECTION=sync"));
        // Order preserved
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "APP_ENV=local");
        assert_eq!(lines[1], "APP_DEBUG=true");
        assert_eq!(lines[2], "QUEUE_CONNECTION=redis");
    }

    #[test]
    fn preserves_comments_and_blank_lines() {
        let input = "# header\nAPP_ENV=local\n\n# group\nQUEUE_CONNECTION=sync\n";
        let (out, _) =
            apply_patches(input, &[EnvPatch::set("QUEUE_CONNECTION", "redis")], "horizon");
        assert!(out.contains("# header\n"));
        assert!(out.contains("\n\n"));
        assert!(out.contains("# group\n"));
    }

    #[test]
    fn appends_new_key_with_hearth_comment() {
        let input = "APP_ENV=local\n";
        let (out, written) = apply_patches(
            input,
            &[EnvPatch::set("TELESCOPE_ENABLED", "true")],
            "telescope",
        );
        assert_eq!(written, vec!["TELESCOPE_ENABLED"]);
        assert!(out.contains("# Added by hearth add telescope"));
        assert!(out.contains("TELESCOPE_ENABLED=true"));
    }

    #[test]
    fn set_if_missing_does_not_overwrite() {
        let input = "REDIS_CLIENT=predis\n";
        let (out, written) = apply_patches(
            input,
            &[EnvPatch::set_if_missing("REDIS_CLIENT", "phpredis")],
            "horizon",
        );
        assert!(written.is_empty());
        assert!(out.contains("REDIS_CLIENT=predis"));
        assert!(!out.contains("phpredis"));
    }

    #[test]
    fn set_if_missing_appends_when_key_absent() {
        let input = "APP_ENV=local\n";
        let (out, written) = apply_patches(
            input,
            &[EnvPatch::set_if_missing("REDIS_CLIENT", "phpredis")],
            "horizon",
        );
        assert_eq!(written, vec!["REDIS_CLIENT"]);
        assert!(out.contains("REDIS_CLIENT=phpredis"));
    }

    #[test]
    fn handles_missing_env_file_via_patch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let env_path = tmp.path().join(".env");
        let result = patch(
            &env_path,
            &[EnvPatch::set("TELESCOPE_ENABLED", "true")],
            "telescope",
        )
        .unwrap();
        assert!(result.backup_path.is_none()); // no original to back up
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(content.contains("TELESCOPE_ENABLED=true"));
    }

    #[test]
    fn creates_backup_before_first_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let env_path = tmp.path().join(".env");
        std::fs::write(&env_path, "APP_ENV=local\n").unwrap();

        let result = patch(
            &env_path,
            &[EnvPatch::set("QUEUE_CONNECTION", "redis")],
            "horizon",
        )
        .unwrap();
        let backup = result.backup_path.unwrap();
        assert!(backup.exists());
        let backup_name = backup.file_name().unwrap().to_string_lossy().to_string();
        assert!(backup_name.starts_with(".env.hearth."));
        assert!(backup_name.ends_with(".bak"));
        // PID portion present (last numeric segment before .bak)
        assert!(
            backup_name
                .trim_end_matches(".bak")
                .split('-')
                .last()
                .unwrap()
                .parse::<u32>()
                .is_ok()
        );
        let backup_content = std::fs::read_to_string(&backup).unwrap();
        assert_eq!(backup_content, "APP_ENV=local\n");
    }

    #[test]
    fn handles_quoted_values() {
        let input = "APP_KEY=\"base64:abc==\"\n";
        let (out, _) = apply_patches(input, &[EnvPatch::set("APP_KEY", "newvalue")], "telescope");
        assert!(out.contains("APP_KEY=newvalue"));
        assert!(!out.contains("base64"));
    }
}
