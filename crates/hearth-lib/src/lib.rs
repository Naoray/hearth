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
/// their paths from this one function.
///
/// This is the RAW derivation seam only — it performs no validation. Every
/// process entrypoint gates on [`validated_config_dir`] first (B5-2: one
/// runtime-mode decision), so no entrypoint ever loads from a path that
/// root construction would reject: in production a present override is
/// accepted only when it is exactly the fixed HOME-derived default, and in
/// typed isolated mode (`HEARTH_ISOLATED_ROOT`) it must validate beneath
/// the isolated boundary.
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
/// instead of silently falling back.
///
/// B5-2: this is the SAME runtime-mode decision as
/// [`php::targets::ProviderRoots::detect`] — the returned path is the
/// validated Hearth config root from provider-root construction itself, so
/// entrypoints and write-authority construction can never disagree about
/// the config location or the runtime mode.
pub fn validated_config_dir() -> Result<PathBuf, String> {
    php::targets::ProviderRoots::detect().map(|roots| roots.hearth.clone())
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
        let base = tmp.path().canonicalize().unwrap();
        let override_dir = base.join("isolated hearth");

        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_CONFIG_DIR",
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
        ]);
        guard.remove("HEARTH_HERD_ROOT");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.set("HEARTH_CONFIG_DIR", &override_dir);

        // Raw derivation seam: every derived path follows the override.
        guard.remove("HEARTH_ISOLATED_ROOT");
        let injected_config = config_dir();
        let injected_socket = crate::socket::socket_path();
        let injected_run = run_dir();
        let injected_log = log_dir();
        let injected_data = data_dir();
        assert_eq!(injected_config, override_dir);
        assert_eq!(injected_socket, override_dir.join("hearth.sock"));
        assert_eq!(injected_run, override_dir.join("run"));
        assert_eq!(injected_log, override_dir.join("log"));
        assert_eq!(injected_data, override_dir.join("data"));

        // B5-2: WITHOUT typed isolated mode, a non-default override is a
        // typed entrypoint error — never an alternate production authority.
        let production = validated_config_dir();
        assert!(
            production.unwrap_err().contains("HEARTH_CONFIG_DIR"),
            "non-default production override must fail closed"
        );

        // WITH typed isolated mode the same override validates beneath the
        // isolated boundary and is returned in normalized form.
        guard.set("HEARTH_ISOLATED_ROOT", &base);
        assert_eq!(validated_config_dir().unwrap(), override_dir);
        drop(guard);

        // Without the override, none of the derived paths point into the
        // injected root (no silent fallback the other way either).
        let default_config = config_dir();
        assert!(!default_config.starts_with(&override_dir));
        assert!(default_config.ends_with("hearth"));
    }

    /// Typed loader fails closed on invalid overrides (B2-6), through the
    /// SAME runtime-mode decision as provider-root construction (B5-2).
    #[test]
    fn validated_config_dir_rejects_empty_and_relative_overrides() {
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_CONFIG_DIR",
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
        ]);
        guard.remove("HEARTH_ISOLATED_ROOT");
        guard.remove("HEARTH_HERD_ROOT");
        guard.remove("HEARTH_HOMEBREW_ROOT");

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
