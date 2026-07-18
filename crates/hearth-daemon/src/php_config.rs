//! Extracted php-config request handling — kept out of `process_request`
//! (cc cap) and testable against a constructed [`DaemonState`] with an
//! injected engine (never the maintainer's real config).

use std::sync::Arc;

use hearth_lib::php::engine::restart_fpm_conditionally;
use hearth_lib::socket::{DaemonResponse, FpmRestartOutcome, PhpConfigAction, PhpScope};

use crate::DaemonState;

/// Handle a V2 php-config action: engine (persist → reconcile), then a
/// conditional FPM restart for mutating actions only.
pub async fn php_config(state: &Arc<DaemonState>, action: PhpConfigAction) -> DaemonResponse {
    let restart = matches!(
        action,
        PhpConfigAction::Set { .. } | PhpConfigAction::Unset { .. }
    );
    match state.php_engine.apply(action).await {
        Err(message) => DaemonResponse::Error { message },
        Ok(mut outcome) => {
            // Hard gate (F4): never restart FPM on unreconciled channel
            // state — the report carries the Refused/Failed files and the
            // CLI renderer exits nonzero.
            if restart && outcome.hard_failures().is_empty() {
                outcome.fpm = restart_fpm_conditionally(&state.supervisor, &state.php_engine).await;
            }
            DaemonResponse::PhpConfigReport(outcome)
        }
    }
}

/// Handle the legacy `PhpConfig { version, key, value }` request (protocol
/// window): map onto a V2 `Set` and answer with a legacy `Ok`/`Error`
/// response so old CLIs keep working against this daemon. The persisted/
/// restart split holds — an unregistered or launch-blocked FPM is reported,
/// not failed; a registered restart failure remains an error.
pub async fn php_config_legacy(
    state: &Arc<DaemonState>,
    version: String,
    key: String,
    value: String,
) -> DaemonResponse {
    let scope = if version == "active" {
        PhpScope::Active
    } else {
        PhpScope::Version { version }
    };
    let action = PhpConfigAction::Set {
        scope,
        key: key.clone(),
        value: value.clone(),
    };
    match php_config(state, action).await {
        DaemonResponse::PhpConfigReport(outcome) => {
            // B2-2: a hard reconciliation failure must surface as a legacy
            // ERROR (paths + reasons), never as an Ok with a soft FPM note.
            // Canonical persistence is mentioned truthfully as partial state,
            // not success.
            let hard = outcome.hard_failures();
            if !hard.is_empty() {
                return DaemonResponse::Error {
                    message: format!(
                        "Set {key}={value}: canonical config persisted, but channel \
                         reconciliation hard-failed and php-fpm was NOT restarted: {}",
                        hard.join("; ")
                    ),
                };
            }
            let fpm_note = match &outcome.fpm {
                FpmRestartOutcome::Restarted => "php-fpm restarted".to_string(),
                FpmRestartOutcome::NotRegistered { herd_hint: true } => {
                    "php-fpm not restarted: not registered (Herd manages PHP-FPM)".to_string()
                }
                FpmRestartOutcome::NotRegistered { herd_hint: false } => {
                    "php-fpm not restarted: not registered".to_string()
                }
                FpmRestartOutcome::LaunchBlocked { reason } => {
                    format!("php-fpm not restarted: launch-blocked ({reason})")
                }
                FpmRestartOutcome::Failed { message } => {
                    return DaemonResponse::Error {
                        message: format!(
                            "Set {key}={value} persisted, but php-fpm restart failed: {message}"
                        ),
                    };
                }
                FpmRestartOutcome::NotAttempted => "php-fpm restart not attempted".to_string(),
            };
            DaemonResponse::Ok {
                message: Some(format!("Set {key}={value} (persisted). {fpm_note}.")),
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::sync::Mutex;

    use hearth_lib::config::HearthConfig;
    use hearth_lib::php::PhpManager;
    use hearth_lib::php::engine::PhpConfigEngine;
    use hearth_lib::php::targets::ProviderRoots;
    use hearth_lib::service::supervisor::ServiceSupervisor;
    use hearth_lib::site::SiteManager;

    /// Constructed daemon state — every path is a tempdir; handlers can never
    /// touch `~/Library` (the S9 hazard this seam exists to remove).
    fn state_fixture() -> (tempfile::TempDir, PathBuf, Arc<DaemonState>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let config_dir = base.join("hearth");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = base.join("config.toml");
        let config = Arc::new(Mutex::new(HearthConfig::default()));
        let roots = ProviderRoots::isolated(
            &base,
            config_dir.clone(),
            base.join("herd"),
            base.join("homebrew"),
        )
        .unwrap();
        let engine = Arc::new(PhpConfigEngine::new(
            Arc::clone(&config),
            config_path.clone(),
            config_dir.clone(),
            roots,
            Arc::new(|| false),
            Arc::new(|_: &hearth_lib::php::targets::PhpTargetIdentity| {
                hearth_lib::php::targets::ExternalFpmEvidence::Unverified {
                    reason: "test".to_string(),
                }
            }),
            Duration::from_millis(50),
        ));
        let state = Arc::new(DaemonState {
            supervisor: Arc::new(Mutex::new(ServiceSupervisor::new())),
            config,
            site_manager: Arc::new(Mutex::new(SiteManager::with_homes(
                vec![base.join("valet")],
                "test".to_string(),
            ))),
            php_manager: Arc::new(Mutex::new(PhpManager::new(config_dir.clone()))),
            add_lock: Arc::new(Mutex::new(())),
            php_engine: engine,
            config_path: config_path.clone(),
        });
        (tmp, config_path, state)
    }

    #[tokio::test]
    async fn v2_set_persists_and_reports_fpm_split() {
        let (_tmp, config_path, state) = state_fixture();
        let response = php_config(
            &state,
            PhpConfigAction::Set {
                scope: hearth_lib::socket::PhpScope::Global,
                key: "memory_limit".to_string(),
                value: "1G".to_string(),
            },
        )
        .await;
        match response {
            DaemonResponse::PhpConfigReport(outcome) => {
                assert_eq!(outcome.persisted, Some(true));
                // No supervisor FPM + no generated fpm config → launch-blocked,
                // never a command failure.
                match outcome.fpm {
                    FpmRestartOutcome::LaunchBlocked { ref reason } => {
                        assert!(reason.contains("#2343"), "got: {reason}");
                    }
                    ref other => panic!("expected LaunchBlocked, got {other:?}"),
                }
            }
            other => panic!("expected PhpConfigReport, got {other:?}"),
        }
        let reloaded = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(
            reloaded.php_ini.global.get("memory_limit"),
            Some(&"1G".to_string())
        );
    }

    #[tokio::test]
    async fn legacy_request_maps_to_set_and_returns_legacy_ok() {
        let (_tmp, config_path, state) = state_fixture();
        let response = php_config_legacy(
            &state,
            "active".to_string(),
            "memory_limit".to_string(),
            "777M".to_string(),
        )
        .await;
        match response {
            DaemonResponse::Ok { message: Some(msg) } => {
                assert!(msg.contains("persisted"), "got: {msg}");
                assert!(msg.contains("php-fpm"), "got: {msg}");
            }
            other => panic!("expected legacy Ok, got {other:?}"),
        }
        // "active" resolved through the default_php version scope.
        let reloaded = HearthConfig::load_from(&config_path).unwrap();
        let default_php = HearthConfig::default().default_php;
        assert_eq!(
            reloaded
                .php_ini
                .overrides
                .get(&default_php)
                .and_then(|m| m.get("memory_limit")),
            Some(&"777M".to_string())
        );
    }

    #[tokio::test]
    async fn legacy_explicit_version_maps_to_version_scope() {
        let (_tmp, config_path, state) = state_fixture();
        let response = php_config_legacy(
            &state,
            "8.2".to_string(),
            "memory_limit".to_string(),
            "256M".to_string(),
        )
        .await;
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let reloaded = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(
            reloaded
                .php_ini
                .overrides
                .get("8.2")
                .and_then(|m| m.get("memory_limit")),
            Some(&"256M".to_string())
        );
    }

    #[tokio::test]
    async fn v2_set_with_untracked_collision_skips_fpm_restart() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, _config_path, state) = state_fixture();
        let base = tmp.path().canonicalize().unwrap();
        // Verified homebrew channel via a fake probe binary…
        let conf_d = base.join("homebrew/etc/php/8.4/conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        let binary = base.join("homebrew/opt/php@8.4/bin/php");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\necho \"Scan for additional .ini files in: {}\"\n",
                conf_d.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::process::Command::new(&binary).arg("--warmup").output();
        // …with an untracked collision in it.
        std::fs::write(conf_d.join("zz-hearth.ini"), "; not ours\n").unwrap();

        let response = php_config(
            &state,
            PhpConfigAction::Set {
                scope: hearth_lib::socket::PhpScope::Global,
                key: "memory_limit".to_string(),
                value: "1G".to_string(),
            },
        )
        .await;
        match response {
            DaemonResponse::PhpConfigReport(outcome) => {
                assert!(
                    !outcome.hard_failures().is_empty(),
                    "collision must be hard: {outcome:?}"
                );
                assert_eq!(
                    outcome.fpm,
                    FpmRestartOutcome::NotAttempted,
                    "no FPM restart on unreconciled channel state"
                );
                assert_eq!(outcome.persisted, Some(true));
            }
            other => panic!("expected PhpConfigReport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_key_yields_error_response_and_no_write() {
        let (_tmp, config_path, state) = state_fixture();
        let response = php_config_legacy(
            &state,
            "active".to_string(),
            "extension".to_string(),
            "evil.so".to_string(),
        )
        .await;
        assert!(matches!(response, DaemonResponse::Error { .. }));
        assert!(!config_path.exists(), "rejected input must not persist");
    }

    fn plant_homebrew_collision(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let base = tmp.path().canonicalize().unwrap();
        let conf_d = base.join("homebrew/etc/php/8.4/conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        let binary = base.join("homebrew/opt/php@8.4/bin/php");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\necho \"Scan for additional .ini files in: {}\"\n",
                conf_d.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::process::Command::new(&binary).arg("--warmup").output();
        let collision = conf_d.join("zz-hearth.ini");
        std::fs::write(&collision, "; not ours\n").unwrap();
        collision
    }

    #[tokio::test]
    async fn legacy_hard_collision_returns_error_no_restart_foreign_untouched() {
        let (tmp, _config_path, state) = state_fixture();
        let collision = plant_homebrew_collision(&tmp);

        let response = php_config_legacy(
            &state,
            "active".to_string(),
            "memory_limit".to_string(),
            "1G".to_string(),
        )
        .await;
        match response {
            DaemonResponse::Error { message } => {
                assert!(message.contains("zz-hearth.ini"), "got: {message}");
                assert!(message.contains("NOT restarted"), "got: {message}");
                assert!(
                    message.contains("persisted"),
                    "partial persistence stated truthfully: {message}"
                );
                assert!(
                    !message
                        .to_lowercase()
                        .starts_with("set memory_limit=1g (persisted). php-fpm"),
                    "must not be success wording"
                );
            }
            other => panic!("expected legacy Error, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&collision).unwrap(),
            "; not ours\n",
            "foreign file untouched"
        );
    }

    #[tokio::test]
    async fn legacy_io_failure_returns_error() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, _config_path, state) = state_fixture();
        let base = tmp.path().canonicalize().unwrap();
        // Hearth-provider fake whose conf.d creation will fail (read-only
        // version dir) → typed Failed → legacy Error.
        let version_dir = base.join("hearth/php/8.4");
        std::fs::create_dir_all(&version_dir).unwrap();
        let binary = version_dir.join("php");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&version_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let response = php_config_legacy(
            &state,
            "active".to_string(),
            "memory_limit".to_string(),
            "1G".to_string(),
        )
        .await;
        std::fs::set_permissions(&version_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        match response {
            DaemonResponse::Error { message } => {
                assert!(
                    message.contains("creation refused") || message.contains("failed"),
                    "got: {message}"
                );
            }
            other => panic!("expected legacy Error, got {other:?}"),
        }
    }
}
