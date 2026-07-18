//! Extracted php-config request handling — kept out of `process_request`
//! (cc cap) and testable against a constructed [`DaemonState`] with an
//! injected engine (never the maintainer's real config).

use std::sync::Arc;
use std::time::SystemTime;

use hearth_lib::php::engine::restart_fpm_conditionally;
use hearth_lib::php::reconcile::{CHANNEL_FILE_NAME, Manifest};
use hearth_lib::service::{ServiceKind, ServiceState};
use hearth_lib::socket::{
    DaemonResponse, FpmRestartOutcome, PhpConfigAction, PhpConfigOutcome, PhpScope,
};

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
            annotate_fpm_runtime(state, &mut outcome).await;
            DaemonResponse::PhpConfigReport(outcome)
        }
    }
}

/// Task 8: truthful runtime annotation for the supervised FPM row
/// (context `launched`). `run_state` comes from the supervisor's own state
/// table; `pending restart` is claimed ONLY when a Hearth-written manifest
/// timestamp is newer than the supervisor's own spawn time for the running
/// FPM — never from ambient process evidence (that class is banned).
/// Annotation is read-only and can never fail the command.
async fn annotate_fpm_runtime(state: &Arc<DaemonState>, outcome: &mut PhpConfigOutcome) {
    let Some(pos) = outcome
        .rows
        .iter()
        .position(|r| r.sapi == "fpm" && r.context == "launched")
    else {
        return;
    };
    let snapshot = {
        let sup = state.supervisor.lock().await;
        sup.service(ServiceKind::PhpFpm).map(|svc| {
            (
                svc.state.clone(),
                svc.started_at(),
                std::path::PathBuf::from(svc.command()),
            )
        })
    };
    let Some((service_state, started_at, supervised_binary)) = snapshot else {
        // Unregistered FPM keeps its truthful static row (LAUNCH-BLOCKED /
        // herd-owned); there is no run state to report.
        return;
    };
    let row = &mut outcome.rows[pos];
    match service_state {
        ServiceState::Running { .. } => {
            row.run_state = Some("running".to_string());
            row.pending_restart = pending_restart_marker(
                channel_applied_at_ms(state, &row.version, &supervised_binary),
                started_at.and_then(unix_ms),
            );
        }
        _ => {
            row.run_state = Some("not running".to_string());
        }
    }
}

/// `pending restart` is truthful only with BOTH timestamps present and the
/// materialization strictly newer than the Hearth-owned spawn time. Any
/// missing side (old manifest without timestamps, no recorded spawn) must
/// stay `false` — never claim staleness that cannot be proven.
fn pending_restart_marker(
    applied_at_unix_ms: Option<u64>,
    started_at_unix_ms: Option<u64>,
) -> bool {
    matches!(
        (applied_at_unix_ms, started_at_unix_ms),
        (Some(applied), Some(started)) if applied > started
    )
}

fn unix_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Newest materialization timestamp among the Hearth-managed channel files
/// the supervised FPM actually loads: always the Hearth conf.d (Belt E scan
/// dir for `version`), plus — when the supervised binary registered by THIS
/// supervisor is a provider build — that provider's verified channel file
/// (C1-5, review 5650). The binary path comes from the supervisor's own
/// registration, and every timestamp is a Hearth-written manifest entry;
/// ambient process evidence is never consulted.
fn channel_applied_at_ms(
    state: &Arc<DaemonState>,
    version: &str,
    supervised_binary: &std::path::Path,
) -> Option<u64> {
    use hearth_lib::php::targets::{PhpProvider, expected_channel_dir};

    let engine = &state.php_engine;
    let roots = engine.provider_roots();
    let mut candidates =
        vec![expected_channel_dir(PhpProvider::Hearth, version, roots).join(CHANNEL_FILE_NAME)];
    let provider = if supervised_binary.starts_with(&roots.herd) {
        Some(PhpProvider::Herd)
    } else if supervised_binary.starts_with(&roots.homebrew) {
        Some(PhpProvider::Homebrew)
    } else {
        None
    };
    if let Some(provider) = provider {
        candidates.push(expected_channel_dir(provider, version, roots).join(CHANNEL_FILE_NAME));
    }
    let manifest = Manifest::load(&engine.manifest_path()).ok()?;
    manifest
        .files
        .iter()
        .filter(|e| candidates.iter().any(|c| c == &e.path))
        .filter_map(|e| e.applied_at_unix_ms)
        .max()
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

    // ---- Task 8: pending-restart marker + run-state annotation ----

    /// Required focused test: the marker is truthful ONLY with both
    /// Hearth-owned timestamps present and a strictly newer materialization.
    #[test]
    fn pending_restart_marker_logic() {
        // Missing either side → never claim staleness.
        assert!(!pending_restart_marker(None, None));
        assert!(!pending_restart_marker(Some(2_000), None));
        assert!(!pending_restart_marker(None, Some(1_000)));
        // Older or equal materialization → the running FPM already has it.
        assert!(!pending_restart_marker(Some(1_000), Some(2_000)));
        assert!(!pending_restart_marker(Some(2_000), Some(2_000)));
        // Strictly newer materialization → restart pending.
        assert!(pending_restart_marker(Some(2_001), Some(2_000)));
    }

    fn launched_row(version: &str) -> hearth_lib::socket::PhpTargetRow {
        hearth_lib::socket::PhpTargetRow {
            provider: "hearth".to_string(),
            version: version.to_string(),
            sapi: "fpm".to_string(),
            context: "launched".to_string(),
            channel: None,
            coverage: "supervised (scan-dir env at launch)".to_string(),
            configured: Some("1G".to_string()),
            observed: Some("1G".to_string()),
            observed_state: "launch-probed".to_string(),
            pending_restart: false,
            run_state: None,
        }
    }

    fn outcome_with(rows: Vec<hearth_lib::socket::PhpTargetRow>) -> PhpConfigOutcome {
        PhpConfigOutcome {
            persisted: None,
            rows,
            files: vec![],
            fpm: FpmRestartOutcome::NotAttempted,
        }
    }

    #[tokio::test]
    async fn annotate_marks_running_fpm_and_pending_restart_from_manifest() {
        use hearth_lib::php::reconcile::{EntryState, LastOutcome, Manifest, ManifestEntry};
        use hearth_lib::service::supervisor::ManagedService;

        let (_tmp, _config_path, state) = state_fixture();
        let default_php = state.config.lock().await.default_php.clone();

        // A real supervisor-spawned child (isolated /bin/sleep) records the
        // trustworthy start time.
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                "/bin/sleep".to_string(),
                vec!["30".to_string()],
            ));
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }

        // Manifest entry materialized BEFORE the spawn → no pending claim.
        let channel_file = state
            .php_engine
            .config_dir()
            .join("php")
            .join(&default_php)
            .join("conf.d")
            .join(CHANNEL_FILE_NAME);
        let manifest_path = state.php_engine.manifest_path();
        let entry = |applied: u64| ManifestEntry {
            path: channel_file.clone(),
            php_version: default_php.clone(),
            channel: "hearth".to_string(),
            sha256: "abc".to_string(),
            state: EntryState::Applied,
            expected_old_sha256: None,
            desired_sha256: None,
            last_outcome: LastOutcome::Written,
            applied_at_unix_ms: Some(applied),
        };
        let save = |applied: u64| {
            let manifest = Manifest {
                version: hearth_lib::php::reconcile::MANIFEST_VERSION,
                files: vec![entry(applied)],
            };
            manifest.save(&manifest_path).unwrap();
        };

        save(1); // long before any possible spawn time
        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        let row = &outcome.rows[0];
        assert_eq!(row.run_state.as_deref(), Some("running"));
        assert!(
            !row.pending_restart,
            "materialized before spawn → already loaded"
        );

        // Materialized AFTER the spawn → truthful pending restart.
        save(i64::MAX as u64); // strictly after any spawn time (TOML-safe)
        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        let row = &outcome.rows[0];
        assert_eq!(row.run_state.as_deref(), Some("running"));
        assert!(row.pending_restart, "newer materialization → pending");

        let mut sup = state.supervisor.lock().await;
        sup.stop_service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap();
    }

    #[tokio::test]
    async fn annotate_marks_registered_stopped_fpm_not_running() {
        use hearth_lib::service::supervisor::ManagedService;

        let (_tmp, _config_path, state) = state_fixture();
        let default_php = state.config.lock().await.default_php.clone();
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                "/bin/sleep".to_string(),
                vec!["30".to_string()],
            ));
            // Registered but never started.
        }
        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        let row = &outcome.rows[0];
        assert_eq!(row.run_state.as_deref(), Some("not running"));
        assert!(!row.pending_restart, "nothing running can be stale");
    }

    /// C1-5 (review 5650): the pending-restart timestamp follows the ACTUAL
    /// supervised binary. A provider-backed FPM (registered command under
    /// the herd root) is pending when the HERD channel file materialized
    /// after spawn; a hearth-backed FPM ignores that unrelated provider
    /// timestamp entirely.
    #[tokio::test]
    async fn annotate_uses_provider_backed_channel_timestamp() {
        use std::os::unix::fs::PermissionsExt;

        use hearth_lib::php::reconcile::{EntryState, LastOutcome, Manifest, ManifestEntry};
        use hearth_lib::php::targets::{PhpProvider, expected_channel_dir};
        use hearth_lib::service::supervisor::ManagedService;

        let (tmp, _config_path, state) = state_fixture();
        let base = tmp.path().canonicalize().unwrap();
        let default_php = state.config.lock().await.default_php.clone();

        // Spawnable fake FPM under the ISOLATED herd root.
        let herd_bin = base.join("herd/bin/php84-fpm");
        std::fs::create_dir_all(herd_bin.parent().unwrap()).unwrap();
        std::fs::write(&herd_bin, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&herd_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                herd_bin.to_string_lossy().to_string(),
                vec![],
            ));
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }

        // Only the HERD channel file has a (newer) materialization stamp.
        let roots = state.php_engine.provider_roots().clone();
        let herd_channel =
            expected_channel_dir(PhpProvider::Herd, &default_php, &roots).join(CHANNEL_FILE_NAME);
        let manifest = Manifest {
            version: hearth_lib::php::reconcile::MANIFEST_VERSION,
            files: vec![ManifestEntry {
                path: herd_channel,
                php_version: default_php.clone(),
                channel: "herd-user".to_string(),
                sha256: "abc".to_string(),
                state: EntryState::Applied,
                expected_old_sha256: None,
                desired_sha256: None,
                last_outcome: LastOutcome::Written,
                applied_at_unix_ms: Some(i64::MAX as u64),
            }],
        };
        manifest.save(&state.php_engine.manifest_path()).unwrap();

        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        assert_eq!(outcome.rows[0].run_state.as_deref(), Some("running"));
        assert!(
            outcome.rows[0].pending_restart,
            "provider-backed FPM must honor its provider channel timestamp"
        );

        // Hearth-backed FPM ignores the unrelated provider timestamp.
        let hearth_bin = state
            .php_engine
            .config_dir()
            .join("php")
            .join(&default_php)
            .join("php-fpm");
        std::fs::create_dir_all(hearth_bin.parent().unwrap()).unwrap();
        std::fs::write(&hearth_bin, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&hearth_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        {
            let mut sup = state.supervisor.lock().await;
            sup.stop_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
            sup.reconfigure_service(
                hearth_lib::service::ServiceKind::PhpFpm,
                hearth_bin.to_string_lossy().to_string(),
                vec![],
                vec![],
            );
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }
        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        assert!(
            !outcome.rows[0].pending_restart,
            "hearth-backed FPM must ignore an unrelated provider channel stamp"
        );

        let mut sup = state.supervisor.lock().await;
        sup.stop_service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap();
    }

    #[tokio::test]
    async fn annotate_leaves_unregistered_fpm_row_untouched() {
        let (_tmp, _config_path, state) = state_fixture();
        let default_php = state.config.lock().await.default_php.clone();
        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        let row = &outcome.rows[0];
        assert_eq!(
            row.run_state, None,
            "unregistered FPM keeps its static truthful row"
        );
        assert!(!row.pending_restart);
    }
}
