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

    /// One test covers the whole seam (env mutation is process-global, so a
    /// single serial test avoids racing parallel tests).
    #[test]
    fn hearth_config_dir_env_overrides_every_derived_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let override_dir = tmp.path().join("isolated hearth");

        // SAFETY: no other test in this crate mutates HEARTH_CONFIG_DIR.
        unsafe { std::env::set_var("HEARTH_CONFIG_DIR", &override_dir) };
        let injected_config = config_dir();
        let injected_socket = crate::socket::socket_path();
        let injected_run = run_dir();
        let injected_log = log_dir();
        let injected_data = data_dir();
        unsafe { std::env::remove_var("HEARTH_CONFIG_DIR") };

        assert_eq!(injected_config, override_dir);
        assert_eq!(injected_socket, override_dir.join("hearth.sock"));
        assert_eq!(injected_run, override_dir.join("run"));
        assert_eq!(injected_log, override_dir.join("log"));
        assert_eq!(injected_data, override_dir.join("data"));

        // Without the override, none of the derived paths point into the
        // injected root (no silent fallback the other way either).
        let default_config = config_dir();
        assert!(!default_config.starts_with(&override_dir));
        assert!(default_config.ends_with("hearth"));
    }
}
