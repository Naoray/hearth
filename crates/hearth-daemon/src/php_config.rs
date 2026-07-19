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
    use hearth_lib::php::reconcile::{EntryState, LastOutcome};
    use hearth_lib::php::targets::{PhpProvider, expected_channel_dir};

    let engine = &state.php_engine;
    let roots = engine.provider_roots();
    // C2-3: canonical, root-bounded, UNAMBIGUOUS provider identity of the
    // exact supervised binary (the supervisor's own registration). Any
    // failure below produces NO claim — never a relabel or a fallback.
    let canonical_binary = supervised_binary.canonicalize().ok()?;
    let mut matched: Vec<(PhpProvider, &'static str, std::path::PathBuf)> = Vec::new();
    for (provider, root, kind) in [
        (PhpProvider::Hearth, &roots.hearth, "hearth"),
        (PhpProvider::Herd, &roots.herd, "herd-user"),
        (PhpProvider::Homebrew, &roots.homebrew, "homebrew"),
    ] {
        let Ok(canonical_root) = root.canonicalize() else {
            continue;
        };
        if canonical_root.parent().is_none() {
            // `/` can never be a provider root — it would match everything.
            continue;
        }
        if canonical_binary.starts_with(&canonical_root) {
            matched.push((provider, kind, canonical_root));
        }
    }
    if matched.len() != 1 {
        // Outside every root, or ambiguous (aliased/nested roots).
        return None;
    }
    let (provider, kind, canonical_provider_root) = matched.remove(0);

    // C3-3: containment is not identity. The supervised command must EQUAL
    // the exact canonical expected provider/version/FPM layout executable —
    // an arbitrary binary elsewhere under the root, a wrong version slot,
    // or a symlink-escaped layout path can never certify pending restart.
    let expected = hearth_lib::php::targets::verify_binary_identity(
        provider,
        version,
        hearth_lib::php::targets::PhpSapi::Fpm,
        roots,
    )
    .ok()?;
    if expected != canonical_binary {
        return None;
    }

    // Exactly ONE expected channel for that identity. The dir must be
    // canonical (no symlink component — Stage-B rule) and must remain inside
    // the canonical provider root; a symlink escape produces no claim.
    let channel_dir = expected_channel_dir(provider, version, roots);
    let canonical_dir = channel_dir.canonicalize().ok()?;
    if canonical_dir != channel_dir || !canonical_dir.starts_with(&canonical_provider_root) {
        return None;
    }

    // Exact-entry match: path + php_version + channel kind + Applied +
    // Written. `Unchanged` is deliberately NOT accepted — the reconcile
    // writer never persists it (unchanged content returns before touching
    // the manifest), so an Applied/Unchanged record is not a genuine durable
    // materialization state. Pending/refused/failed/removed/unrelated
    // entries are ignored; no newest-of-many selection exists.
    let channel_file = channel_dir.join(CHANNEL_FILE_NAME);
    let manifest = Manifest::load(&engine.manifest_path()).ok()?;
    let entry = manifest.files.iter().find(|e| {
        e.path == channel_file
            && e.php_version == version
            && e.channel == kind
            && e.state == EntryState::Applied
            && matches!(e.last_outcome, LastOutcome::Written)
    })?;

    // C3-3 live evidence: the manifest record alone is not proof. The
    // channel file must exist NOW, must not be a symlink, and its current
    // bytes must hash to the recorded applied hash — a removed, replaced,
    // or externally modified file voids the pending claim until reconcile
    // repairs it.
    let meta = std::fs::symlink_metadata(&channel_file).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    let bytes = std::fs::read(&channel_file).ok()?;
    if hearth_lib::php::reconcile::sha256_hex(&bytes) != entry.sha256 {
        return None;
    }
    entry.applied_at_unix_ms
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
                FpmRestartOutcome::SkippedHerdOwned => {
                    "php-fpm restart skipped: Herd owns PHP-FPM (registered service untouched)"
                        .to_string()
                }
                FpmRestartOutcome::SkippedOwnershipUnknown { reason } => format!(
                    "php-fpm restart skipped: ownership unknown ({reason}) — \
                     activation fails closed"
                ),
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
        state_fixture_with_probe(Arc::new(|| {
            hearth_lib::service::supervisor::FpmOwnership::Unowned
        }))
    }

    /// Flippable-ownership daemon fixture (C2-1): Herd can become live AFTER
    /// the supervisor registered/started FPM.
    fn state_fixture_with_herd_flag() -> (
        tempfile::TempDir,
        PathBuf,
        Arc<DaemonState>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_flag = Arc::clone(&flag);
        let (tmp, config_path, state) = state_fixture_with_probe(Arc::new(move || {
            if probe_flag.load(std::sync::atomic::Ordering::SeqCst) {
                hearth_lib::service::supervisor::FpmOwnership::Owned
            } else {
                hearth_lib::service::supervisor::FpmOwnership::Unowned
            }
        }));
        (tmp, config_path, state, flag)
    }

    fn state_fixture_with_probe(
        herd: hearth_lib::php::engine::HerdProbe,
    ) -> (tempfile::TempDir, PathBuf, Arc<DaemonState>) {
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
            herd,
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

    /// C2-1 (review 5650 r2): the FULL daemon Set path — persistence,
    /// reconcile, restart policy — must not stop/start/restart/register/
    /// remove a registered running FPM once Herd became live, must report
    /// the truthful skip, and must leak no process. Fails at head 08b7232.
    #[tokio::test]
    async fn daemon_set_skips_registered_running_fpm_after_herd_becomes_live() {
        use hearth_lib::service::supervisor::ManagedService;
        use std::sync::atomic::Ordering;

        let (_tmp, config_path, state, herd_live) = state_fixture_with_herd_flag();
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                "/bin/sleep".to_string(),
                vec!["30".to_string()],
            ))
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }
        let started_before = state
            .supervisor
            .lock()
            .await
            .service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        herd_live.store(true, Ordering::SeqCst);

        let response = php_config(
            &state,
            PhpConfigAction::Set {
                scope: PhpScope::Global,
                key: "memory_limit".to_string(),
                value: "1G".to_string(),
            },
        )
        .await;
        match response {
            DaemonResponse::PhpConfigReport(outcome) => {
                assert_eq!(outcome.persisted, Some(true), "persistence proceeds");
                assert_eq!(
                    outcome.fpm,
                    FpmRestartOutcome::SkippedHerdOwned,
                    "truthful restart-skipped status"
                );
            }
            other => panic!("expected PhpConfigReport, got {other:?}"),
        }
        // Config really persisted.
        let reloaded = HearthConfig::load_from(&config_path).unwrap();
        assert_eq!(
            reloaded.php_ini.global.get("memory_limit"),
            Some(&"1G".to_string())
        );
        // Supervisor state untouched: same registration, same spawn, running.
        {
            let sup = state.supervisor.lock().await;
            let svc = sup
                .service(hearth_lib::service::ServiceKind::PhpFpm)
                .expect("registration survives");
            assert!(matches!(
                svc.state,
                hearth_lib::service::ServiceState::Running { .. }
            ));
            assert_eq!(
                svc.started_at(),
                Some(started_before),
                "no stop/start/restart happened"
            );
        }
        // No process leak: stop the reviewer-owned child explicitly.
        state
            .supervisor
            .lock()
            .await
            .stop_service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap();
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
            status_key: None,
            status_token: None,
        }
    }

    #[tokio::test]
    async fn annotate_marks_running_fpm_and_pending_restart_from_manifest() {
        use hearth_lib::php::reconcile::{EntryState, LastOutcome, Manifest, ManifestEntry};
        use hearth_lib::service::supervisor::ManagedService;

        let (_tmp, _config_path, state) = state_fixture();
        let default_php = state.config.lock().await.default_php.clone();

        // A real supervisor-spawned child — the HEARTH-root fake FPM, so the
        // C2-3 canonical provider identity resolves to the hearth channel —
        // records the trustworthy start time.
        let hearth_bin = state
            .php_engine
            .config_dir()
            .join("php")
            .join(&default_php)
            .join("php-fpm");
        write_spawnable(&hearth_bin);
        let conf_d = state
            .php_engine
            .config_dir()
            .join("php")
            .join(&default_php)
            .join("conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                hearth_bin.to_string_lossy().to_string(),
                vec![],
            ))
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }

        // Manifest entry materialized BEFORE the spawn → no pending claim.
        // C3-3: live evidence — the channel file really exists and hashes to
        // the recorded value.
        let live_sha = write_live_channel(&conf_d);
        let channel_file = conf_d.join(CHANNEL_FILE_NAME);
        let manifest_path = state.php_engine.manifest_path();
        let entry = |applied: u64| ManifestEntry {
            path: channel_file.clone(),
            php_version: default_php.clone(),
            channel: "hearth".to_string(),
            sha256: live_sha.clone(),
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
            ))
            .unwrap();
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
            ))
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }

        // Only the HERD channel file has a (newer) materialization stamp.
        // The channel dir must really exist (C2-3: canonical containment is
        // checked against the actual dir, not a reconstructed suffix).
        let roots = state.php_engine.provider_roots().clone();
        let herd_dir = expected_channel_dir(PhpProvider::Herd, &default_php, &roots);
        let live_sha = write_live_channel(&herd_dir);
        let herd_channel = herd_dir.join(CHANNEL_FILE_NAME);
        let manifest = Manifest {
            version: hearth_lib::php::reconcile::MANIFEST_VERSION,
            files: vec![ManifestEntry {
                path: herd_channel,
                php_version: default_php.clone(),
                channel: "herd-user".to_string(),
                sha256: live_sha,
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
            )
            .unwrap();
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

    /// Spawnable fake service binary (real child process, isolated path).
    fn write_spawnable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// C3-3 live evidence: write a real channel file and return its sha256 —
    /// the hash a truthful manifest entry must record.
    fn write_live_channel(dir: &std::path::Path) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let content = b"; managed by hearth\nmemory_limit=1G\n";
        std::fs::write(dir.join(CHANNEL_FILE_NAME), content).unwrap();
        hearth_lib::php::reconcile::sha256_hex(content)
    }

    // ---- C2-3 (review 5650 r2): exact-entry + root-bounded identity ----

    fn herd_entry(
        path: std::path::PathBuf,
        version: &str,
        sha256: &str,
        state: hearth_lib::php::reconcile::EntryState,
        outcome: hearth_lib::php::reconcile::LastOutcome,
    ) -> hearth_lib::php::reconcile::ManifestEntry {
        hearth_lib::php::reconcile::ManifestEntry {
            path,
            php_version: version.to_string(),
            channel: "herd-user".to_string(),
            sha256: sha256.to_string(),
            state,
            expected_old_sha256: None,
            desired_sha256: None,
            last_outcome: outcome,
            applied_at_unix_ms: Some(i64::MAX as u64),
        }
    }

    /// The exact-entry rules: a provider-backed FPM ignores unrelated newer
    /// Hearth entries; wrong php_version, non-Applied state, and non-success
    /// outcomes are all ignored; the exact legitimate entry still claims
    /// pending. Fails at head 08b7232 (newest-of-many by path alone).
    #[tokio::test]
    async fn pending_restart_requires_exact_entry_match() {
        use hearth_lib::php::reconcile::{EntryState, LastOutcome, Manifest, ManifestEntry};
        use hearth_lib::php::targets::{PhpProvider, expected_channel_dir};
        use hearth_lib::service::supervisor::ManagedService;

        let (tmp, _config_path, state) = state_fixture();
        let base = tmp.path().canonicalize().unwrap();
        let default_php = state.config.lock().await.default_php.clone();
        let roots = state.php_engine.provider_roots().clone();

        // Provider-backed supervised FPM (herd root), running.
        let herd_bin = base.join("herd/bin/php84-fpm");
        write_spawnable(&herd_bin);
        let herd_dir = expected_channel_dir(PhpProvider::Herd, &default_php, &roots);
        std::fs::create_dir_all(&herd_dir).unwrap();
        let herd_file = herd_dir.join(CHANNEL_FILE_NAME);
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                herd_bin.to_string_lossy().to_string(),
                vec![],
            ))
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }

        let manifest_path = state.php_engine.manifest_path();
        let save = |files: Vec<ManifestEntry>| {
            Manifest {
                version: hearth_lib::php::reconcile::MANIFEST_VERSION,
                files,
            }
            .save(&manifest_path)
            .unwrap();
        };
        async fn pending_now(state: &Arc<DaemonState>, version: &str) -> bool {
            let mut outcome = outcome_with(vec![launched_row(version)]);
            annotate_fpm_runtime(state, &mut outcome).await;
            outcome.rows[0].pending_restart
        }

        // Live channel evidence for every otherwise-plausible herd entry.
        let live_sha = write_live_channel(&herd_dir);

        // Unrelated NEWER Hearth entry alone: ignored by a provider-backed
        // FPM (with real live evidence on the hearth side too).
        let hearth_conf_d = state
            .php_engine
            .config_dir()
            .join("php")
            .join(&default_php)
            .join("conf.d");
        let hearth_sha = write_live_channel(&hearth_conf_d);
        let hearth_file = hearth_conf_d.join(CHANNEL_FILE_NAME);
        save(vec![ManifestEntry {
            path: hearth_file,
            php_version: default_php.clone(),
            channel: "hearth".to_string(),
            sha256: hearth_sha,
            state: EntryState::Applied,
            expected_old_sha256: None,
            desired_sha256: None,
            last_outcome: LastOutcome::Written,
            applied_at_unix_ms: Some(i64::MAX as u64),
        }]);
        assert!(
            !pending_now(&state, &default_php).await,
            "provider-backed FPM must ignore an unrelated newer Hearth entry"
        );

        // Wrong php_version on the exact path: ignored.
        save(vec![herd_entry(
            herd_file.clone(),
            "8.3",
            &live_sha,
            EntryState::Applied,
            LastOutcome::Written,
        )]);
        assert!(
            !pending_now(&state, &default_php).await,
            "wrong php_version must be ignored"
        );

        // Pending (journaled, not applied): ignored.
        save(vec![herd_entry(
            herd_file.clone(),
            &default_php,
            &live_sha,
            EntryState::Pending,
            LastOutcome::Written,
        )]);
        assert!(
            !pending_now(&state, &default_php).await,
            "journaled-but-unapplied entries must be ignored"
        );

        // Refused/failed outcomes: ignored.
        for outcome_kind in [LastOutcome::Refused, LastOutcome::Failed] {
            save(vec![herd_entry(
                herd_file.clone(),
                &default_php,
                &live_sha,
                EntryState::Applied,
                outcome_kind,
            )]);
            assert!(
                !pending_now(&state, &default_php).await,
                "non-success outcomes must be ignored"
            );
        }

        // Removed/empty manifest: no claim.
        save(vec![]);
        assert!(!pending_now(&state, &default_php).await);

        // The exact legitimate provider entry still claims pending.
        save(vec![herd_entry(
            herd_file.clone(),
            &default_php,
            &live_sha,
            EntryState::Applied,
            LastOutcome::Written,
        )]);
        assert!(
            pending_now(&state, &default_php).await,
            "the exact provider entry newer than spawn → pending"
        );

        state
            .supervisor
            .lock()
            .await
            .stop_service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap();
    }

    /// A symlinked expected channel dir that escapes the provider root can
    /// never feed a pending claim (canonical/root-bounded rule).
    #[tokio::test]
    async fn pending_restart_rejects_symlinked_channel_escape() {
        use hearth_lib::php::reconcile::{EntryState, LastOutcome};
        use hearth_lib::php::targets::{PhpProvider, expected_channel_dir};
        use hearth_lib::service::supervisor::ManagedService;

        let (tmp, _config_path, state) = state_fixture();
        let base = tmp.path().canonicalize().unwrap();
        let default_php = state.config.lock().await.default_php.clone();
        let roots = state.php_engine.provider_roots().clone();

        let herd_bin = base.join("herd/bin/php84-fpm");
        write_spawnable(&herd_bin);
        // The expected herd channel dir is a SYMLINK escaping the herd root.
        let outside = base.join("outside-channel");
        std::fs::create_dir_all(&outside).unwrap();
        let herd_dir = expected_channel_dir(PhpProvider::Herd, &default_php, &roots);
        std::fs::create_dir_all(herd_dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &herd_dir).unwrap();
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                herd_bin.to_string_lossy().to_string(),
                vec![],
            ))
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }
        // Even a perfect-looking manifest entry on the literal path is void.
        let herd_file = herd_dir.join(CHANNEL_FILE_NAME);
        hearth_lib::php::reconcile::Manifest {
            version: hearth_lib::php::reconcile::MANIFEST_VERSION,
            files: vec![herd_entry(
                herd_file,
                &default_php,
                "deadbeef",
                EntryState::Applied,
                LastOutcome::Written,
            )],
        }
        .save(&state.php_engine.manifest_path())
        .unwrap();

        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        assert!(
            !outcome.rows[0].pending_restart,
            "a symlink-escaped channel can never feed a pending claim"
        );
        state
            .supervisor
            .lock()
            .await
            .stop_service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap();
    }

    /// Aliased provider roots (herd root symlinked onto the hearth root)
    /// make the binary's provider identity ambiguous — no claim.
    #[tokio::test]
    async fn pending_restart_rejects_aliased_ambiguous_roots() {
        use hearth_lib::php::reconcile::{EntryState, LastOutcome, Manifest, ManifestEntry};
        use hearth_lib::service::supervisor::ManagedService;

        let (tmp, _config_path, state) = state_fixture();
        let base = tmp.path().canonicalize().unwrap();
        let default_php = state.config.lock().await.default_php.clone();

        // Alias: base/herd → base/hearth (the hearth config dir). The
        // hearth-root binary now lies under BOTH canonical roots.
        std::os::unix::fs::symlink(state.php_engine.config_dir(), base.join("herd")).unwrap();
        let hearth_bin = state
            .php_engine
            .config_dir()
            .join("php")
            .join(&default_php)
            .join("php-fpm");
        write_spawnable(&hearth_bin);
        let conf_d = state
            .php_engine
            .config_dir()
            .join("php")
            .join(&default_php)
            .join("conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                hearth_bin.to_string_lossy().to_string(),
                vec![],
            ))
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }
        // A perfectly valid hearth entry WITH live evidence exists —
        // ambiguity alone still voids it.
        let live_sha = write_live_channel(&conf_d);
        Manifest {
            version: hearth_lib::php::reconcile::MANIFEST_VERSION,
            files: vec![ManifestEntry {
                path: conf_d.join(CHANNEL_FILE_NAME),
                php_version: default_php.clone(),
                channel: "hearth".to_string(),
                sha256: live_sha,
                state: EntryState::Applied,
                expected_old_sha256: None,
                desired_sha256: None,
                last_outcome: LastOutcome::Written,
                applied_at_unix_ms: Some(i64::MAX as u64),
            }],
        }
        .save(&state.php_engine.manifest_path())
        .unwrap();

        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        assert!(
            !outcome.rows[0].pending_restart,
            "ambiguous provider identity must produce no pending claim"
        );
        state
            .supervisor
            .lock()
            .await
            .stop_service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap();
    }

    // ---- C3-3 (review 5650 r3): exact layout + live materialization evidence ----

    /// Containment is not identity: an arbitrary executable elsewhere under
    /// the provider root, or an exact-layout path that is a symlink escaping
    /// the root, can never certify pending restart. Fails at head 7c88222.
    #[tokio::test]
    async fn pending_restart_requires_exact_fpm_layout() {
        use hearth_lib::php::reconcile::{EntryState, LastOutcome, Manifest};
        use hearth_lib::php::targets::{PhpProvider, expected_channel_dir};
        use hearth_lib::service::supervisor::ManagedService;

        let (tmp, _config_path, state) = state_fixture();
        let base = tmp.path().canonicalize().unwrap();
        let default_php = state.config.lock().await.default_php.clone();
        let roots = state.php_engine.provider_roots().clone();

        // Perfect live channel + perfect manifest entry.
        let herd_dir = expected_channel_dir(PhpProvider::Herd, &default_php, &roots);
        let live_sha = write_live_channel(&herd_dir);
        let herd_file = herd_dir.join(CHANNEL_FILE_NAME);
        let save_entry = || {
            Manifest {
                version: hearth_lib::php::reconcile::MANIFEST_VERSION,
                files: vec![herd_entry(
                    herd_file.clone(),
                    &default_php,
                    &live_sha,
                    EntryState::Applied,
                    LastOutcome::Written,
                )],
            }
            .save(&state.php_engine.manifest_path())
            .unwrap();
        };
        save_entry();

        // An ARBITRARY executable under the herd root — not the expected
        // php{XY}-fpm layout slot.
        let rogue = base.join("herd/libexec/php-fpm");
        write_spawnable(&rogue);
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                rogue.to_string_lossy().to_string(),
                vec![],
            ))
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }
        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        assert!(
            !outcome.rows[0].pending_restart,
            "an arbitrary executable under the root is not the provider's FPM"
        );

        // The exact-layout slot as a SYMLINK escaping the herd root.
        let outside_bin = base.join("outside-fpm");
        write_spawnable(&outside_bin);
        let layout_slot = base.join("herd/bin/php84-fpm");
        std::fs::create_dir_all(layout_slot.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside_bin, &layout_slot).unwrap();
        {
            let mut sup = state.supervisor.lock().await;
            sup.stop_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
            sup.reconfigure_service(
                hearth_lib::service::ServiceKind::PhpFpm,
                layout_slot.to_string_lossy().to_string(),
                vec![],
                vec![],
            )
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }
        save_entry();
        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        assert!(
            !outcome.rows[0].pending_restart,
            "a symlink-escaped layout slot can never certify pending"
        );

        // Positive control: the REAL exact-layout binary certifies.
        std::fs::remove_file(&layout_slot).unwrap();
        write_spawnable(&layout_slot);
        {
            let mut sup = state.supervisor.lock().await;
            sup.stop_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
            sup.reconfigure_service(
                hearth_lib::service::ServiceKind::PhpFpm,
                layout_slot.to_string_lossy().to_string(),
                vec![],
                vec![],
            )
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }
        save_entry();
        let mut outcome = outcome_with(vec![launched_row(&default_php)]);
        annotate_fpm_runtime(&state, &mut outcome).await;
        assert!(
            outcome.rows[0].pending_restart,
            "the exact provider/version/FPM layout binary still certifies"
        );

        state
            .supervisor
            .lock()
            .await
            .stop_service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap();
    }

    /// The manifest record alone is not proof: the channel file must exist
    /// NOW, be a regular file, and hash to the recorded applied hash.
    /// `Unchanged` — never persisted by the writer — certifies nothing.
    /// Fails at head 7c88222.
    #[tokio::test]
    async fn pending_restart_requires_live_channel_evidence() {
        use hearth_lib::php::reconcile::{EntryState, LastOutcome, Manifest};
        use hearth_lib::php::targets::{PhpProvider, expected_channel_dir};
        use hearth_lib::service::supervisor::ManagedService;

        let (tmp, _config_path, state) = state_fixture();
        let base = tmp.path().canonicalize().unwrap();
        let default_php = state.config.lock().await.default_php.clone();
        let roots = state.php_engine.provider_roots().clone();

        let herd_bin = base.join("herd/bin/php84-fpm");
        write_spawnable(&herd_bin);
        let herd_dir = expected_channel_dir(PhpProvider::Herd, &default_php, &roots);
        let live_sha = write_live_channel(&herd_dir);
        let herd_file = herd_dir.join(CHANNEL_FILE_NAME);
        {
            let mut sup = state.supervisor.lock().await;
            sup.register(ManagedService::new(
                hearth_lib::service::ServiceKind::PhpFpm,
                herd_bin.to_string_lossy().to_string(),
                vec![],
            ))
            .unwrap();
            sup.start_service(hearth_lib::service::ServiceKind::PhpFpm)
                .unwrap();
        }
        let save_with = |state_kind, outcome_kind| {
            Manifest {
                version: hearth_lib::php::reconcile::MANIFEST_VERSION,
                files: vec![herd_entry(
                    herd_file.clone(),
                    &default_php,
                    &live_sha,
                    state_kind,
                    outcome_kind,
                )],
            }
            .save(&state.php_engine.manifest_path())
            .unwrap();
        };
        async fn pending(state: &Arc<DaemonState>, version: &str) -> bool {
            let mut outcome = outcome_with(vec![launched_row(version)]);
            annotate_fpm_runtime(state, &mut outcome).await;
            outcome.rows[0].pending_restart
        }

        // Baseline: live file + exact Written entry → pending.
        save_with(EntryState::Applied, LastOutcome::Written);
        assert!(pending(&state, &default_php).await, "baseline certifies");

        // `Unchanged` is never a durable writer state → no claim.
        save_with(EntryState::Applied, LastOutcome::Unchanged);
        assert!(
            !pending(&state, &default_php).await,
            "Unchanged must not certify"
        );

        // Missing channel file with a stale Applied/Written entry → no claim.
        save_with(EntryState::Applied, LastOutcome::Written);
        std::fs::remove_file(&herd_file).unwrap();
        assert!(
            !pending(&state, &default_php).await,
            "a removed channel file voids the stale record"
        );

        // Externally modified file (hash mismatch) → no claim.
        std::fs::write(&herd_file, "; tampered\n").unwrap();
        assert!(
            !pending(&state, &default_php).await,
            "a modified channel file voids the record"
        );

        // Symlink replacement → no claim, even with matching bytes behind it.
        std::fs::remove_file(&herd_file).unwrap();
        let decoy = base.join("decoy.ini");
        std::fs::write(&decoy, b"; managed by hearth\nmemory_limit=1G\n").unwrap();
        std::os::unix::fs::symlink(&decoy, &herd_file).unwrap();
        assert!(
            !pending(&state, &default_php).await,
            "a symlinked channel file can never certify"
        );

        // Restore the genuine file → certifies again.
        std::fs::remove_file(&herd_file).unwrap();
        let restored_sha = write_live_channel(&herd_dir);
        assert_eq!(restored_sha, live_sha, "same bytes, same hash");
        assert!(pending(&state, &default_php).await);

        state
            .supervisor
            .lock()
            .await
            .stop_service(hearth_lib::service::ServiceKind::PhpFpm)
            .unwrap();
    }
}
