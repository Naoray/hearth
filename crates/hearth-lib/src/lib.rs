pub mod add;
pub mod config;
pub mod db;
pub mod download;
pub mod dump;
pub(crate) mod fsync;
pub mod mailpit;
pub mod mcp;
pub mod php;
pub mod service;
pub mod site;
pub mod socket;
#[cfg(test)]
pub(crate) mod test_env;
pub mod valet;

use std::path::PathBuf;

/// Hearth's configuration directory — THE runtime-path seam.
///
/// Default: `~/Library/Application Support/hearth` via the `dirs` crate,
/// which resolves the macOS home from the user database (`getpwuid`), NOT
/// the `HOME` environment variable. The `HEARTH_CONFIG_DIR` environment
/// variable overrides it explicitly and deterministically: the daemon, CLI,
/// MCP server, socket path, run/log/data dirs, config.toml (and therefore
/// every configurable port), `php exec`, and the smoke harness all derive
/// their paths from this one function, so tests and smoke runs can isolate
/// completely without touching real user state.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HEARTH_CONFIG_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    dirs::config_dir()
        .expect("could not determine config directory")
        .join("hearth")
}

/// Typed runtime-path loader for process entrypoints (daemon boot, CLI
/// commands that construct an engine). Fails closed on an invalid override
/// instead of silently falling back: an empty or relative
/// `HEARTH_CONFIG_DIR` is a configuration error, never a default.
pub fn validated_config_dir() -> Result<PathBuf, String> {
    match std::env::var_os("HEARTH_CONFIG_DIR") {
        None => Ok(config_dir()),
        Some(dir) if dir.is_empty() => Err(
            "HEARTH_CONFIG_DIR is set but empty — unset it or point it at an absolute directory"
                .to_string(),
        ),
        Some(dir) => {
            let path = PathBuf::from(dir);
            if !path.is_absolute() {
                return Err(format!(
                    "HEARTH_CONFIG_DIR must be an absolute path, got {}",
                    path.display()
                ));
            }
            Ok(path)
        }
    }
}

/// Default runtime directory for PID files and socket
pub fn run_dir() -> PathBuf {
    config_dir().join("run")
}

/// Default log directory
pub fn log_dir() -> PathBuf {
    config_dir().join("log")
}

/// Default data directory (parent of per-engine datadirs).
pub fn data_dir() -> PathBuf {
    config_dir().join("data")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env mutation goes through the serialized, panic-safe EnvGuard.
    #[test]
    fn hearth_config_dir_env_overrides_every_derived_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let override_dir = tmp.path().join("isolated hearth");

        let guard = crate::test_env::EnvGuard::capture(["HEARTH_CONFIG_DIR"]);
        guard.set("HEARTH_CONFIG_DIR", &override_dir);
        let injected_config = config_dir();
        let injected_socket = crate::socket::socket_path();
        let injected_run = run_dir();
        let injected_log = log_dir();
        let injected_data = data_dir();
        let validated = validated_config_dir();
        drop(guard);

        assert_eq!(injected_config, override_dir);
        assert_eq!(injected_socket, override_dir.join("hearth.sock"));
        assert_eq!(injected_run, override_dir.join("run"));
        assert_eq!(injected_log, override_dir.join("log"));
        assert_eq!(injected_data, override_dir.join("data"));
        assert_eq!(validated.unwrap(), override_dir);

        // Without the override, none of the derived paths point into the
        // injected root (no silent fallback the other way either).
        let default_config = config_dir();
        assert!(!default_config.starts_with(&override_dir));
        assert!(default_config.ends_with("hearth"));
    }

    /// Typed loader fails closed on invalid overrides (B2-6).
    #[test]
    fn validated_config_dir_rejects_empty_and_relative_overrides() {
        let guard = crate::test_env::EnvGuard::capture(["HEARTH_CONFIG_DIR"]);

        guard.set("HEARTH_CONFIG_DIR", "");
        let empty = validated_config_dir();
        assert!(empty.unwrap_err().contains("empty"));

        guard.set("HEARTH_CONFIG_DIR", "relative/dir");
        let relative = validated_config_dir();
        assert!(relative.unwrap_err().contains("absolute"));

        guard.remove("HEARTH_CONFIG_DIR");
        assert!(validated_config_dir().is_ok());
        drop(guard);
    }
}
