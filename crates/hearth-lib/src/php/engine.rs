//! Shared php-config engine — the single implementation behind the daemon
//! socket handler and the MCP tools (S9 seam from plan 5560 §2.6).
//!
//! Every dependency that could touch the maintainer's real machine is
//! injected: config path, config dir, provider roots, Herd probe, probe
//! timeout. Tests run entirely against tempdirs. The internal `op_lock`
//! serializes config+reconcile mutations across the daemon and MCP handlers;
//! it is acquired before any state lock, and the architecture lock order
//! (`config → php_manager → site_manager → supervisor`) is preserved — the
//! config mutex is never held across a probe await (snapshot, drop, probe).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use crate::config::{HearthConfig, PhpIniSettings};
use crate::php::ini_guard;
use crate::php::reconcile::{
    self, CHANNEL_FILE_NAME, EntryState, LastOutcome, Manifest, WriteOutcome, render_ini,
    sha256_hex,
};
use crate::php::targets::{
    ChannelClass, PhpProvider, PhpSapi, PhpTarget, ProbeInfo, ProviderRoots, classify_channel,
    discover_provider_targets, probe_scan_dirs,
};
use crate::service::ServiceKind;
use crate::service::supervisor::ServiceSupervisor;
use crate::socket::{
    FileOutcome, FileWriteResult, FpmRestartOutcome, PhpConfigAction, PhpConfigOutcome, PhpScope,
    PhpTargetRow,
};

/// Injected Herd-ownership probe (production: `service::manager::is_herd_running`).
pub type HerdProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// Channel-file materialization state for one expected file, used to join
/// classification with outcome (tier ⊓ outcome — status never overclaims).
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileState {
    Applied,
    SyncPending,
    Blocked(String),
}

pub struct PhpConfigEngine {
    config: Arc<Mutex<HearthConfig>>,
    config_path: PathBuf,
    config_dir: PathBuf,
    provider_roots: ProviderRoots,
    herd_running: HerdProbe,
    probe_timeout: Duration,
    /// Serializes config/reconcile mutations across daemon + MCP handlers.
    op_lock: Mutex<()>,
    /// Probe cache keyed on `(binary, mtime)` — a replaced binary re-probes.
    probe_cache: std::sync::Mutex<HashMap<(PathBuf, Option<SystemTime>), ProbeInfo>>,
}

impl PhpConfigEngine {
    pub fn new(
        config: Arc<Mutex<HearthConfig>>,
        config_path: PathBuf,
        config_dir: PathBuf,
        provider_roots: ProviderRoots,
        herd_running: HerdProbe,
        probe_timeout: Duration,
    ) -> Self {
        Self {
            config,
            config_path,
            config_dir,
            provider_roots,
            herd_running,
            probe_timeout,
            op_lock: Mutex::new(()),
            probe_cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.config_dir.join("php").join("manifest.toml")
    }

    pub fn herd_running(&self) -> bool {
        (self.herd_running)()
    }

    /// `Some(reason)` when Hearth's own FPM cannot launch because nothing has
    /// generated its config yet (todo #2343 owns generation).
    pub fn fpm_launch_blocked_reason(&self) -> Option<String> {
        let conf = self.config_dir.join("fpm").join("php-fpm.conf");
        if conf.is_file() {
            None
        } else {
            Some("missing fpm config — see todo #2343".to_string())
        }
    }

    /// Execute one typed action. All mutating actions are serialized by the
    /// engine op lock. `fpm` in the returned outcome is always `NotAttempted`;
    /// the caller that owns the supervisor decides on a restart (see
    /// [`restart_fpm_conditionally`]).
    pub async fn apply(&self, action: PhpConfigAction) -> Result<PhpConfigOutcome, String> {
        let _op = self.op_lock.lock().await;
        match action {
            PhpConfigAction::Set { scope, key, value } => {
                self.mutate(scope, &key, Some(&value)).await
            }
            PhpConfigAction::Unset { scope, key } => self.mutate(scope, &key, None).await,
            PhpConfigAction::Show { key } => self.observe(key.as_deref()).await,
            PhpConfigAction::Status => self.observe(None).await,
            PhpConfigAction::Sync => self.sync_locked().await,
            PhpConfigAction::Unmanage => self.unmanage_locked(),
        }
    }

    /// Daemon-boot hook: journal recovery + guarded legacy migration +
    /// reconcile, before `default_services` builds the supervisor.
    pub async fn boot_sync(&self) -> Result<PhpConfigOutcome, String> {
        let _op = self.op_lock.lock().await;
        self.sync_locked().await
    }

    /// Set (`value: Some`) or Unset (`value: None`) one directive, persist the
    /// config to the injected path, then reconcile channel files.
    async fn mutate(
        &self,
        scope: PhpScope,
        key: &str,
        value: Option<&str>,
    ) -> Result<PhpConfigOutcome, String> {
        ini_guard::validate_key(key).map_err(|e| e.to_string())?;
        if let Some(v) = value {
            ini_guard::validate_value(v).map_err(|e| e.to_string())?;
        }
        if let PhpScope::Version { version } = &scope {
            validate_version(version)?;
        }

        let (snapshot, default_php) = {
            let mut cfg = self.config.lock().await;
            let version = match &scope {
                PhpScope::Global => None,
                PhpScope::Version { version } => Some(version.clone()),
                PhpScope::Active => Some(cfg.default_php.clone()),
            };
            let prior = cfg.php_ini.clone();
            match (&version, value) {
                (None, Some(v)) => {
                    cfg.php_ini.global.insert(key.to_string(), v.to_string());
                }
                (Some(ver), Some(v)) => {
                    cfg.php_ini
                        .overrides
                        .entry(ver.clone())
                        .or_default()
                        .insert(key.to_string(), v.to_string());
                }
                (None, None) => {
                    cfg.php_ini.global.remove(key);
                }
                (Some(ver), None) => {
                    if let Some(map) = cfg.php_ini.overrides.get_mut(ver) {
                        map.remove(key);
                        if map.is_empty() {
                            cfg.php_ini.overrides.remove(ver);
                        }
                    }
                }
            }
            if let Err(e) = cfg.save_to(&self.config_path) {
                cfg.php_ini = prior;
                return Err(format!("failed to persist config: {e}"));
            }
            (cfg.php_ini.clone(), cfg.default_php.clone())
        };
        // Config lock dropped — probes and reconcile run without it.

        let (targets, creation_failures) = self.build_targets(&snapshot, true).await;
        let report = reconcile::reconcile(
            &snapshot,
            &targets,
            &self.manifest_path(),
            &self.provider_roots,
        );
        let mut file_states = file_states_from_report(&report);
        for failure in &creation_failures {
            if let FileWriteResult::Failed { error } = &failure.result {
                file_states.insert(
                    PathBuf::from(&failure.path),
                    FileState::Blocked(error.clone()),
                );
            }
        }
        let mut files: Vec<FileOutcome> = report
            .files
            .iter()
            .map(|action| FileOutcome {
                path: action.path.to_string_lossy().to_string(),
                result: wire_result(&action.outcome),
            })
            .collect();
        files.extend(creation_failures);
        let rows = self.build_rows(&snapshot, &default_php, &targets, Some(key), &file_states);

        Ok(PhpConfigOutcome {
            persisted: Some(true),
            rows,
            files,
            fpm: FpmRestartOutcome::NotAttempted,
        })
    }

    /// Show/Status: read-only report. Never writes; degraded channel state is
    /// reported as `sync pending` / `channel blocked`, never repaired here.
    async fn observe(&self, key: Option<&str>) -> Result<PhpConfigOutcome, String> {
        let (snapshot, default_php) = {
            let cfg = self.config.lock().await;
            (cfg.php_ini.clone(), cfg.default_php.clone())
        };
        let (targets, _) = self.build_targets(&snapshot, false).await;
        let file_states = self.file_states_from_manifest(&snapshot, &targets);
        let rows = self.build_rows(&snapshot, &default_php, &targets, key, &file_states);
        Ok(PhpConfigOutcome {
            persisted: None,
            rows,
            files: Vec::new(),
            fpm: FpmRestartOutcome::NotAttempted,
        })
    }

    /// Journal recovery → guarded legacy migration → reconcile.
    async fn sync_locked(&self) -> Result<PhpConfigOutcome, String> {
        reconcile::recover_pending(&self.manifest_path())
            .map_err(|e| format!("journal recovery failed: {e}"))?;

        let (snapshot, default_php) = {
            let mut cfg = self.config.lock().await;
            let php_dir = self.config_dir.join("php");
            reconcile::migrate_legacy_inis(&mut cfg, &self.config_path, &php_dir)
                .map_err(|e| format!("legacy INI migration failed: {e}"))?;
            (cfg.php_ini.clone(), cfg.default_php.clone())
        };

        let (targets, creation_failures) = self.build_targets(&snapshot, true).await;
        let report = reconcile::reconcile(
            &snapshot,
            &targets,
            &self.manifest_path(),
            &self.provider_roots,
        );
        let mut file_states = file_states_from_report(&report);
        for failure in &creation_failures {
            if let FileWriteResult::Failed { error } = &failure.result {
                file_states.insert(
                    PathBuf::from(&failure.path),
                    FileState::Blocked(error.clone()),
                );
            }
        }
        let mut files: Vec<FileOutcome> = report
            .files
            .iter()
            .map(|action| FileOutcome {
                path: action.path.to_string_lossy().to_string(),
                result: wire_result(&action.outcome),
            })
            .collect();
        files.extend(creation_failures);
        let rows = self.build_rows(&snapshot, &default_php, &targets, None, &file_states);

        Ok(PhpConfigOutcome {
            persisted: None,
            rows,
            files,
            fpm: FpmRestartOutcome::NotAttempted,
        })
    }

    /// Remove every manifest-tracked channel file (journal-resolved,
    /// exact-hash only — survivors are reported Refused).
    fn unmanage_locked(&self) -> Result<PhpConfigOutcome, String> {
        let actions = reconcile::unmanage(&self.manifest_path()).map_err(|e| e.to_string())?;
        let files = actions
            .iter()
            .map(|action| FileOutcome {
                path: action.path.to_string_lossy().to_string(),
                result: wire_result(&action.outcome),
            })
            .collect();
        Ok(PhpConfigOutcome {
            persisted: None,
            rows: Vec::new(),
            files,
            fpm: FpmRestartOutcome::NotAttempted,
        })
    }

    /// Discover + probe + classify every provider target — READ-ONLY unless
    /// `create_missing` is set (mutating hooks only). Ambient (Herd/Homebrew)
    /// channels must preexist and are never created; a missing Hearth-owned
    /// conf.d for a configured version is created exclusively through the
    /// verified [`crate::php::targets::ensure_hearth_channel_dir`] mechanism,
    /// and a refused or failed creation is returned as a typed `Failed` file
    /// outcome so the central hard-failure gate fires (the Hearth channel is
    /// promised hard-guarantee coverage).
    async fn build_targets(
        &self,
        php_ini: &PhpIniSettings,
        create_missing: bool,
    ) -> (Vec<PhpTarget>, Vec<FileOutcome>) {
        let ids = discover_provider_targets(&self.provider_roots);

        let mut creation_failures: Vec<FileOutcome> = Vec::new();
        if create_missing {
            let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            for id in &ids {
                if id.provider == PhpProvider::Hearth
                    && !php_ini.effective_for(&id.version).is_empty()
                {
                    let conf_d = self
                        .provider_roots
                        .hearth
                        .join("php")
                        .join(&id.version)
                        .join("conf.d");
                    if !seen.insert(conf_d.clone()) || conf_d.is_dir() {
                        continue;
                    }
                    if let Err(rejection) = crate::php::targets::ensure_hearth_channel_dir(
                        &self.provider_roots.hearth,
                        &conf_d,
                    ) {
                        creation_failures.push(FileOutcome {
                            path: conf_d.to_string_lossy().to_string(),
                            result: FileWriteResult::Failed {
                                error: format!("channel dir creation refused: {rejection}"),
                            },
                        });
                    }
                }
            }
        }

        let mut targets = Vec::with_capacity(ids.len());
        for id in ids {
            let probe = self.probe_cached(&id.binary, id.sapi).await;
            let normal_channel = classify_channel(
                id.provider,
                &id.version,
                probe.normal_scan_dir.as_deref(),
                context_failure(&probe, "normal probe:"),
                &self.provider_roots,
            );
            let sanitized_channel = classify_channel(
                id.provider,
                &id.version,
                probe.sanitized_scan_dir.as_deref(),
                context_failure(&probe, "sanitized probe:"),
                &self.provider_roots,
            );
            let write_channel = match (&normal_channel, &sanitized_channel) {
                (ChannelClass::Verified { dir }, _) => Some(dir.clone()),
                (_, ChannelClass::Verified { dir }) => Some(dir.clone()),
                _ => None,
            };
            targets.push(PhpTarget {
                id,
                probe,
                normal_channel,
                sanitized_channel,
                write_channel,
            });
        }
        (targets, creation_failures)
    }

    async fn probe_cached(&self, binary: &Path, sapi: PhpSapi) -> ProbeInfo {
        let mtime = std::fs::metadata(binary).and_then(|m| m.modified()).ok();
        let cache_key = (binary.to_path_buf(), mtime);
        if let Some(hit) = self
            .probe_cache
            .lock()
            .expect("probe cache poisoned")
            .get(&cache_key)
        {
            return hit.clone();
        }
        let info = probe_scan_dirs(binary, sapi, self.probe_timeout).await;
        self.probe_cache
            .lock()
            .expect("probe cache poisoned")
            .insert(cache_key, info.clone());
        info
    }

    /// Expected-file materialization states from the manifest (read-only).
    fn file_states_from_manifest(
        &self,
        php_ini: &PhpIniSettings,
        targets: &[PhpTarget],
    ) -> HashMap<PathBuf, FileState> {
        let manifest = Manifest::load(&self.manifest_path()).unwrap_or_default();
        let mut states = HashMap::new();
        for target in targets {
            let Some(dir) = &target.write_channel else {
                continue;
            };
            let effective = php_ini.effective_for(&target.id.version);
            if effective.is_empty() {
                continue;
            }
            let file = dir.join(CHANNEL_FILE_NAME);
            if states.contains_key(&file) {
                continue;
            }
            let desired_sha = sha256_hex(render_ini(&effective).as_bytes());
            let state = match manifest.files.iter().find(|e| e.path == file) {
                Some(entry) => match entry.last_outcome {
                    LastOutcome::Refused => {
                        FileState::Blocked("last reconcile refused (collision)".to_string())
                    }
                    LastOutcome::Failed => FileState::Blocked("last reconcile failed".to_string()),
                    _ if entry.state == EntryState::Applied && entry.sha256 == desired_sha => {
                        FileState::Applied
                    }
                    _ => FileState::SyncPending,
                },
                None => FileState::SyncPending,
            };
            states.insert(file, state);
        }
        states
    }

    /// One row per (target, context), plus the truthful Hearth-FPM service
    /// row. Coverage = classification ⊓ channel-file outcome; env credit only
    /// where the canary passed.
    fn build_rows(
        &self,
        php_ini: &PhpIniSettings,
        default_php: &str,
        targets: &[PhpTarget],
        key: Option<&str>,
        file_states: &HashMap<PathBuf, FileState>,
    ) -> Vec<PhpTargetRow> {
        let mut rows = Vec::new();
        for target in targets {
            let effective = php_ini.effective_for(&target.id.version);
            let managed_expected = !effective.is_empty();
            let configured = key.and_then(|k| effective.get(k).cloned());
            for (context, class) in [
                ("normal", &target.normal_channel),
                ("sanitized", &target.sanitized_channel),
            ] {
                let channel = match class {
                    ChannelClass::Verified { dir } | ChannelClass::PrivilegedDir { dir } => {
                        Some(dir.to_string_lossy().to_string())
                    }
                    ChannelClass::BestEffort { .. } => None,
                };
                let file_state = match class {
                    ChannelClass::Verified { dir } => file_states.get(&dir.join(CHANNEL_FILE_NAME)),
                    _ => None,
                };
                let coverage = coverage_label(
                    context,
                    class,
                    target.probe.env_honored,
                    file_state,
                    managed_expected,
                );
                let materialized = managed_expected
                    && matches!(class, ChannelClass::Verified { .. })
                    && matches!(file_state, Some(FileState::Applied));
                let (observed, observed_state) = match (&configured, materialized) {
                    (Some(value), true) => (Some(value.clone()), "materialized".to_string()),
                    _ => (None, "n/a".to_string()),
                };
                rows.push(PhpTargetRow {
                    provider: provider_str(target.id.provider).to_string(),
                    version: target.id.version.clone(),
                    sapi: sapi_str(target.id.sapi).to_string(),
                    context: context.to_string(),
                    channel,
                    coverage,
                    configured: configured.clone(),
                    observed,
                    observed_state,
                });
            }
        }

        // Hearth's own supervised FPM service — truthfully LAUNCH-BLOCKED
        // while nothing generates its config (todo #2343). Herd running means
        // Herd owns FPM and no Hearth FPM service row exists.
        if !self.herd_running() {
            let effective = php_ini.effective_for(default_php);
            let coverage = if let Some(reason) = self.fpm_launch_blocked_reason() {
                format!("LAUNCH-BLOCKED: {reason}")
            } else if crate::php::resolver::resolve_phpfpm_binary(default_php, &self.config_dir)
                .is_none()
            {
                format!("LAUNCH-BLOCKED: php-fpm binary for {default_php} not found")
            } else {
                "supervised (scan-dir env at launch)".to_string()
            };
            rows.push(PhpTargetRow {
                provider: "hearth".to_string(),
                version: default_php.to_string(),
                sapi: "fpm".to_string(),
                context: "launched".to_string(),
                channel: None,
                coverage,
                configured: key.and_then(|k| effective.get(k).cloned()),
                observed: None,
                observed_state: "n/a".to_string(),
            });
        }
        rows
    }
}

/// Restart the supervised php-fpm only when it is actually registered.
/// Unregistered/optional FPM is informational (`NotRegistered`/`LaunchBlocked`),
/// never a command failure; a registered restart failure stays a hard `Failed`.
pub async fn restart_fpm_conditionally(
    supervisor: &Arc<Mutex<ServiceSupervisor>>,
    engine: &PhpConfigEngine,
) -> FpmRestartOutcome {
    let mut sup = supervisor.lock().await;
    if !sup.status().contains_key(&ServiceKind::PhpFpm) {
        drop(sup);
        if engine.herd_running() {
            return FpmRestartOutcome::NotRegistered { herd_hint: true };
        }
        if let Some(reason) = engine.fpm_launch_blocked_reason() {
            return FpmRestartOutcome::LaunchBlocked { reason };
        }
        return FpmRestartOutcome::NotRegistered { herd_hint: false };
    }
    if let Err(e) = sup.stop_service(ServiceKind::PhpFpm) {
        return FpmRestartOutcome::Failed {
            message: format!("php-fpm stop failed: {e}"),
        };
    }
    match sup.start_service(ServiceKind::PhpFpm) {
        Ok(()) => FpmRestartOutcome::Restarted,
        Err(e) => FpmRestartOutcome::Failed {
            message: format!("php-fpm start failed: {e}"),
        },
    }
}

/// Central hard-failure gate (review 5594 F4). Converts an engine outcome
/// into an error when reconciliation hard-failed: outer errors pass through;
/// hard-guarantee target `Refused`/`Failed` outcomes become an `Err` carrying
/// every path + reason. Every PHP launch/restart hook (daemon boot, add
/// pre-launch, `php exec`, install, daemon/MCP switch and config) MUST route
/// its sync through this and abort the launch on `Err`. The canonical config
/// stays truthfully persisted either way — only materialization failed.
pub fn require_reconciled(
    result: Result<PhpConfigOutcome, String>,
) -> Result<PhpConfigOutcome, String> {
    let outcome = result?;
    let hard = outcome.hard_failures();
    if hard.is_empty() {
        Ok(outcome)
    } else {
        Err(format!(
            "php-config reconciliation hard failure — PHP launch/restart aborted \
             (canonical config remains persisted): {}",
            hard.join("; ")
        ))
    }
}

/// Shared PHP version switch used by BOTH the daemon socket handler and the
/// MCP tool (review 5594 F3): persists `default_php` via the injected engine
/// paths, hard-gates the reconcile, then REBUILDS the FPM service from
/// scratch for the new version — binary, args, and scan-dir env — via
/// `manager::hearth_fpm_service`. A launch-blocked or unresolvable FPM
/// deregisters the old service truthfully instead of leaving a stale one.
pub async fn switch_php_version(
    supervisor: &Arc<Mutex<ServiceSupervisor>>,
    engine: &PhpConfigEngine,
    version: &str,
) -> Result<String, String> {
    validate_version(version)?;
    let config_dir = engine.config_dir().to_path_buf();
    if crate::php::resolver::resolve_php_binary(version, &config_dir).is_none()
        && crate::php::resolver::resolve_phpfpm_binary(version, &config_dir).is_none()
    {
        return Err(format!(
            "PHP {version} is not installed (no CLI or FPM binary found)"
        ));
    }

    {
        let mut cfg = engine.config.lock().await;
        cfg.default_php = version.to_string();
        cfg.save_to(engine.config_path())
            .map_err(|e| format!("failed to save config: {e}"))?;
    }
    // Reconcile channel files for the new active version BEFORE any
    // supervisor acquisition (lock order), hard-gated: a Refused/Failed
    // channel aborts the FPM restart.
    require_reconciled(engine.apply(crate::socket::PhpConfigAction::Sync).await)
        .map_err(|e| format!("switched default_php to {version} (persisted), but: {e}"))?;

    let mut sup = supervisor.lock().await;
    let _ = sup.stop_service(ServiceKind::PhpFpm);
    match crate::service::manager::hearth_fpm_service(version, &config_dir) {
        Ok(svc) => {
            sup.register(svc);
            sup.start_service(ServiceKind::PhpFpm)
                .map_err(|e| format!("switched to PHP {version}, but php-fpm start failed: {e}"))?;
            Ok(format!("Switched to PHP {version}; php-fpm restarted"))
        }
        Err(reason) => {
            let removed = sup.remove_service(ServiceKind::PhpFpm);
            let herd_note = if engine.herd_running() {
                " (Herd manages PHP-FPM)"
            } else {
                ""
            };
            let stale_note = if removed {
                "; previous php-fpm registration removed"
            } else {
                ""
            };
            Ok(format!(
                "Switched to PHP {version}; php-fpm not registered: {reason}{herd_note}{stale_note}"
            ))
        }
    }
}

fn validate_version(version: &str) -> Result<(), String> {
    let plausible = !version.is_empty()
        && version.chars().all(|c| c.is_ascii_digit() || c == '.')
        && version.contains('.');
    if plausible {
        Ok(())
    } else {
        Err(format!(
            "invalid PHP version `{version}` — expected e.g. 8.4"
        ))
    }
}

fn provider_str(provider: PhpProvider) -> &'static str {
    match provider {
        PhpProvider::Hearth => "hearth",
        PhpProvider::Herd => "herd",
        PhpProvider::Homebrew => "homebrew",
    }
}

fn sapi_str(sapi: PhpSapi) -> &'static str {
    match sapi {
        PhpSapi::Cli => "cli",
        PhpSapi::Fpm => "fpm",
    }
}

fn context_failure<'a>(info: &'a ProbeInfo, prefix: &str) -> Option<&'a str> {
    info.failures
        .iter()
        .find(|f| f.starts_with(prefix))
        .map(String::as_str)
}

fn wire_result(outcome: &WriteOutcome) -> FileWriteResult {
    match outcome {
        WriteOutcome::Written => FileWriteResult::Written,
        WriteOutcome::Unchanged => FileWriteResult::Unchanged,
        WriteOutcome::Deleted => FileWriteResult::Deleted,
        WriteOutcome::Refused { reason } => FileWriteResult::Refused {
            reason: reason.clone(),
        },
        WriteOutcome::Failed { error } => FileWriteResult::Failed {
            error: error.clone(),
        },
    }
}

fn file_states_from_report(report: &reconcile::ReconcileReport) -> HashMap<PathBuf, FileState> {
    report
        .files
        .iter()
        .map(|action| {
            let state = match &action.outcome {
                WriteOutcome::Written | WriteOutcome::Unchanged | WriteOutcome::Deleted => {
                    FileState::Applied
                }
                WriteOutcome::Refused { reason } => FileState::Blocked(reason.clone()),
                WriteOutcome::Failed { error } => FileState::Blocked(error.clone()),
            };
            (action.path.clone(), state)
        })
        .collect()
}

/// Truthful coverage label for one (context, classification, outcome) cell.
/// Never claims universal coverage; degraded channels are loud.
fn coverage_label(
    context: &str,
    class: &ChannelClass,
    env_honored: bool,
    file_state: Option<&FileState>,
    managed_expected: bool,
) -> String {
    match class {
        ChannelClass::Verified { dir } => {
            let base = if context == "normal" {
                if env_honored {
                    "managed (channel+env)"
                } else {
                    "managed (channel; env ignored)"
                }
            } else {
                "managed (channel)"
            };
            if managed_expected {
                match file_state {
                    Some(FileState::Blocked(_)) => {
                        return format!(
                            "managed* — channel blocked: {}",
                            dir.join(CHANNEL_FILE_NAME).display()
                        );
                    }
                    Some(FileState::SyncPending) => return format!("{base} (sync pending)"),
                    _ => {}
                }
            }
            base.to_string()
        }
        ChannelClass::PrivilegedDir { .. } => "UNMANAGED: privileged-dir".to_string(),
        ChannelClass::BestEffort { reason } => format!("unmanaged: best-effort ({reason})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HearthConfig;
    use std::sync::Arc;

    struct Fixture {
        _tmp: tempfile::TempDir,
        engine: Arc<PhpConfigEngine>,
        config: Arc<Mutex<HearthConfig>>,
        config_path: PathBuf,
        config_dir: PathBuf,
    }

    fn fixture(herd: bool) -> Fixture {
        // Canonicalized base — /var → /private/var on macOS would otherwise
        // break allowlist prefix checks.
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let config_dir = base.join("hearth config"); // path with a space
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = base.join("config.toml");
        let roots = ProviderRoots {
            hearth: config_dir.clone(),
            herd: base.join("herd"),
            homebrew: base.join("homebrew"),
        };
        let config = Arc::new(Mutex::new(HearthConfig::default()));
        let engine = Arc::new(PhpConfigEngine::new(
            Arc::clone(&config),
            config_path.clone(),
            config_dir.clone(),
            roots,
            Arc::new(move || herd),
            Duration::from_millis(50),
        ));
        Fixture {
            _tmp: tmp,
            engine,
            config,
            config_path,
            config_dir,
        }
    }

    fn set_global(key: &str, value: &str) -> PhpConfigAction {
        PhpConfigAction::Set {
            scope: PhpScope::Global,
            key: key.to_string(),
            value: value.to_string(),
        }
    }

    #[tokio::test]
    async fn set_persists_even_when_fpm_unregistered() {
        let fx = fixture(false);
        // FPM config exists → plain NotRegistered, not LaunchBlocked.
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();

        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert_eq!(outcome.persisted, Some(true));
        assert_eq!(outcome.fpm, FpmRestartOutcome::NotAttempted);

        // Persisted to the injected temp path, never the real config.
        let reloaded = HearthConfig::load_from(&fx.config_path).unwrap();
        assert_eq!(
            reloaded.php_ini.global.get("memory_limit"),
            Some(&"1G".to_string())
        );

        // Empty supervisor → informational NotRegistered, not a failure.
        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        let fpm = restart_fpm_conditionally(&supervisor, &fx.engine).await;
        assert_eq!(fpm, FpmRestartOutcome::NotRegistered { herd_hint: false });
    }

    #[tokio::test]
    async fn unregistered_fpm_with_missing_conf_is_launch_blocked() {
        let fx = fixture(false);
        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        let fpm = restart_fpm_conditionally(&supervisor, &fx.engine).await;
        match fpm {
            FpmRestartOutcome::LaunchBlocked { reason } => {
                assert!(
                    reason.contains("#2343"),
                    "reason must cite todo #2343: {reason}"
                );
            }
            other => panic!("expected LaunchBlocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unregistered_fpm_under_herd_hints_herd() {
        let fx = fixture(true);
        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        let fpm = restart_fpm_conditionally(&supervisor, &fx.engine).await;
        assert_eq!(fpm, FpmRestartOutcome::NotRegistered { herd_hint: true });
    }

    #[tokio::test]
    async fn set_invalid_key_rejected_nothing_written() {
        let fx = fixture(false);
        for key in ["extension", "SENDMAIL_PATH", "bad key"] {
            let err = fx.engine.apply(set_global(key, "x")).await.unwrap_err();
            assert!(!err.is_empty());
        }
        // Injection in the value is rejected too.
        let err = fx
            .engine
            .apply(set_global("memory_limit", "1G\nextension=evil.so"))
            .await
            .unwrap_err();
        assert!(!err.is_empty());

        assert!(
            !fx.config_path.exists(),
            "rejected input must not persist anything"
        );
        assert!(fx.config.lock().await.php_ini.global.is_empty());
    }

    #[tokio::test]
    async fn active_scope_resolves_default_php() {
        let fx = fixture(false);
        fx.config.lock().await.default_php = "8.3".to_string();

        let outcome = fx
            .engine
            .apply(PhpConfigAction::Set {
                scope: PhpScope::Active,
                key: "memory_limit".to_string(),
                value: "2G".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(outcome.persisted, Some(true));

        let reloaded = HearthConfig::load_from(&fx.config_path).unwrap();
        assert_eq!(
            reloaded
                .php_ini
                .overrides
                .get("8.3")
                .and_then(|m| m.get("memory_limit")),
            Some(&"2G".to_string())
        );
        assert!(reloaded.php_ini.global.is_empty());
    }

    #[tokio::test]
    async fn unset_removes_key_and_prunes_empty_override() {
        let fx = fixture(false);
        fx.engine
            .apply(PhpConfigAction::Set {
                scope: PhpScope::Version {
                    version: "8.4".to_string(),
                },
                key: "memory_limit".to_string(),
                value: "2G".to_string(),
            })
            .await
            .unwrap();
        fx.engine
            .apply(PhpConfigAction::Unset {
                scope: PhpScope::Version {
                    version: "8.4".to_string(),
                },
                key: "memory_limit".to_string(),
            })
            .await
            .unwrap();

        let reloaded = HearthConfig::load_from(&fx.config_path).unwrap();
        assert!(
            reloaded.php_ini.overrides.is_empty(),
            "empty override table must be pruned"
        );
    }

    #[tokio::test]
    async fn concurrent_ops_serialized_by_op_lock() {
        let fx = fixture(false);
        let guard = fx.engine.op_lock.lock().await;

        let engine = Arc::clone(&fx.engine);
        let task =
            tokio::spawn(async move { engine.apply(set_global("memory_limit", "512M")).await });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !task.is_finished(),
            "apply must wait for the engine op lock"
        );

        drop(guard);
        let outcome = task.await.unwrap().unwrap();
        assert_eq!(outcome.persisted, Some(true));
    }

    #[tokio::test]
    async fn status_reports_hearth_fpm_launch_blocked_with_todo() {
        let fx = fixture(false);
        let outcome = fx.engine.apply(PhpConfigAction::Status).await.unwrap();
        let fpm_row = outcome
            .rows
            .iter()
            .find(|r| r.context == "launched" && r.sapi == "fpm")
            .expect("hearth fpm service row expected when Herd is not running");
        assert!(
            fpm_row.coverage.starts_with("LAUNCH-BLOCKED"),
            "got: {}",
            fpm_row.coverage
        );
        assert!(
            fpm_row.coverage.contains("#2343"),
            "got: {}",
            fpm_row.coverage
        );
    }

    #[tokio::test]
    async fn status_under_herd_has_no_hearth_fpm_service_row() {
        let fx = fixture(true);
        let outcome = fx.engine.apply(PhpConfigAction::Status).await.unwrap();
        assert!(outcome.rows.iter().all(|r| r.context != "launched"));
    }

    #[tokio::test]
    async fn sync_migrates_legacy_ini_into_overrides() {
        let fx = fixture(false);
        let legacy_dir = fx.config_dir.join("php").join("8.4");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("php.ini"), "[PHP]\nmemory_limit=1G\n").unwrap();

        let outcome = fx.engine.apply(PhpConfigAction::Sync).await.unwrap();
        assert_eq!(outcome.persisted, None);

        let cfg = fx.config.lock().await;
        assert_eq!(
            cfg.php_ini
                .overrides
                .get("8.4")
                .and_then(|m| m.get("memory_limit")),
            Some(&"1G".to_string())
        );
        drop(cfg);
        assert!(
            !legacy_dir.join("php.ini").exists(),
            "legacy ini must be renamed after migration"
        );
        assert!(legacy_dir.join("php.ini.migrated.bak").exists());
    }

    #[tokio::test]
    async fn unmanage_on_empty_manifest_is_clean() {
        let fx = fixture(false);
        let outcome = fx.engine.apply(PhpConfigAction::Unmanage).await.unwrap();
        assert!(outcome.files.is_empty());
        assert_eq!(outcome.persisted, None);
    }

    fn write_executable(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // One-time unbounded warm-up: macOS syspolicyd can stall the first
        // exec of a fresh script past the injected probe timeout.
        let _ = std::process::Command::new(path).arg("--warmup").output();
    }

    /// Fake Homebrew PHP whose probes report the given scan dir in every
    /// context — yields a Verified channel for that dir.
    fn install_fake_homebrew_php(fx: &Fixture, version: &str) -> PathBuf {
        let roots_homebrew = fx.engine.provider_roots.homebrew.clone();
        let conf_d = roots_homebrew.join("etc/php").join(version).join("conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        let binary = roots_homebrew
            .join("opt")
            .join(format!("php@{version}"))
            .join("bin/php");
        write_executable(
            &binary,
            &format!(
                "#!/bin/sh\necho \"Scan for additional .ini files in: {}\"\n",
                conf_d.display()
            ),
        );
        conf_d
    }

    #[test]
    fn require_reconciled_gates_hard_outcomes() {
        use crate::socket::{FileOutcome, FileWriteResult};
        let clean = PhpConfigOutcome {
            persisted: Some(true),
            rows: vec![],
            files: vec![FileOutcome {
                path: "/x/zz-hearth.ini".to_string(),
                result: FileWriteResult::Written,
            }],
            fpm: FpmRestartOutcome::LaunchBlocked {
                reason: "missing fpm config — see todo #2343".to_string(),
            },
        };
        assert!(
            require_reconciled(Ok(clean)).is_ok(),
            "informational cells are not hard"
        );

        let refused = PhpConfigOutcome {
            persisted: Some(true),
            rows: vec![],
            files: vec![FileOutcome {
                path: "/chan/zz-hearth.ini".to_string(),
                result: FileWriteResult::Refused {
                    reason: "untracked collision".to_string(),
                },
            }],
            fpm: FpmRestartOutcome::NotAttempted,
        };
        let err = require_reconciled(Ok(refused)).unwrap_err();
        assert!(err.contains("/chan/zz-hearth.ini"), "got: {err}");
        assert!(err.contains("untracked collision"), "got: {err}");
        assert!(err.contains("persisted"), "got: {err}");

        let failed = PhpConfigOutcome {
            persisted: None,
            rows: vec![],
            files: vec![FileOutcome {
                path: "/chan/conf.d".to_string(),
                result: FileWriteResult::Failed {
                    error: "io".to_string(),
                },
            }],
            fpm: FpmRestartOutcome::NotAttempted,
        };
        assert!(require_reconciled(Ok(failed)).is_err());

        assert_eq!(
            require_reconciled(Err("outer".to_string())).unwrap_err(),
            "outer"
        );
    }

    #[tokio::test]
    async fn set_with_untracked_collision_is_hard_refused_and_gated() {
        let fx = fixture(false);
        let conf_d = install_fake_homebrew_php(&fx, "8.4");
        // Untracked collision: a foreign zz-hearth.ini with no manifest entry.
        let collision = conf_d.join("zz-hearth.ini");
        std::fs::write(&collision, "; not ours\n").unwrap();

        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert_eq!(
            outcome.persisted,
            Some(true),
            "config stays truthfully persisted"
        );
        let hard = outcome.hard_failures();
        assert!(
            !hard.is_empty(),
            "collision must be a hard failure: {outcome:?}"
        );
        assert!(hard[0].contains("zz-hearth.ini"), "got: {hard:?}");
        // The foreign file was never touched.
        assert_eq!(std::fs::read_to_string(&collision).unwrap(), "; not ours\n");
        // And the central gate converts it into an abort.
        assert!(require_reconciled(Ok(outcome)).is_err());
    }

    #[tokio::test]
    async fn hearth_symlink_ancestor_escape_is_hard_failure_zero_mutation() {
        let fx = fixture(false);
        // hearth `php` component is a symlink escaping the root.
        let outside = fx
            ._tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("outside-target");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fx.engine.provider_roots.hearth.join("php")).unwrap();
        // A discoverable hearth binary through the symlink.
        write_executable(
            &fx.engine.provider_roots.hearth.join("php/8.4/php"),
            "#!/bin/sh\nexit 0\n",
        );

        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        let hard = outcome.hard_failures();
        assert!(
            hard.iter()
                .any(|h| h.contains("creation refused") && h.contains("symlink")),
            "symlinked ancestor must be a typed hard failure: {hard:?}"
        );
        assert!(
            !outside.join("8.4/conf.d").exists(),
            "zero mutation outside the allowlist"
        );
        assert!(require_reconciled(Ok(outcome)).is_err());
    }

    #[tokio::test]
    async fn hearth_channel_creation_failure_is_typed_and_gated() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture(false);
        let version_dir = fx.engine.provider_roots.hearth.join("php/8.4");
        write_executable(&version_dir.join("php"), "#!/bin/sh\nexit 0\n");
        // Read-only version dir: ancestor checks pass (owned, not group/other
        // writable) but conf.d creation fails with EACCES.
        std::fs::set_permissions(&version_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        std::fs::set_permissions(&version_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hard = outcome.hard_failures();
        assert!(
            hard.iter().any(|h| h.contains("creation refused")),
            "creation failure must be typed + hard: {hard:?}"
        );
        assert!(require_reconciled(Ok(outcome)).is_err());
    }

    #[tokio::test]
    async fn observe_never_creates_hearth_channel_dirs() {
        let fx = fixture(false);
        write_executable(
            &fx.engine.provider_roots.hearth.join("php/8.4/php"),
            "#!/bin/sh\nexit 0\n",
        );
        fx.config
            .lock()
            .await
            .php_ini
            .global
            .insert("memory_limit".to_string(), "1G".to_string());

        let outcome = fx.engine.apply(PhpConfigAction::Status).await.unwrap();
        assert!(outcome.files.is_empty(), "status is read-only");
        assert!(
            !fx.engine
                .provider_roots
                .hearth
                .join("php/8.4/conf.d")
                .exists(),
            "discovery/probing/classification must not create directories"
        );
    }

    fn write_fake_fpm_pair(fx: &Fixture, version: &str) {
        // Long-running fake so start_service succeeds and supervision works.
        write_executable(
            &fx.config_dir.join("php").join(version).join("php-fpm"),
            "#!/bin/sh\nsleep 30\n",
        );
    }

    #[tokio::test]
    async fn switch_rebuilds_fpm_service_with_new_version_env() {
        let fx = fixture(false);
        write_fake_fpm_pair(&fx, "8.3");
        write_fake_fpm_pair(&fx, "8.4");
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();
        fx.config.lock().await.default_php = "8.3".to_string();

        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        {
            // Old registration carrying the OLD version's env.
            let mut sup = supervisor.lock().await;
            sup.register(
                crate::service::manager::hearth_fpm_service("8.3", &fx.config_dir).unwrap(),
            );
            let old = sup.service(ServiceKind::PhpFpm).unwrap();
            assert!(old.env()[0].1.contains("8.3"));
        }

        let message = switch_php_version(&supervisor, &fx.engine, "8.4")
            .await
            .unwrap();
        assert!(message.contains("Switched to PHP 8.4"), "got: {message}");

        let mut sup = supervisor.lock().await;
        let svc = sup.service(ServiceKind::PhpFpm).expect("fpm registered");
        assert!(
            svc.command().ends_with("php/8.4/php-fpm"),
            "new binary expected, got: {}",
            svc.command()
        );
        assert_eq!(
            svc.env(),
            &[crate::php::scan_dir_env(&fx.config_dir, "8.4")],
            "env must be the NEW version's scan dir, never the old one"
        );
        // Persisted through the injected path.
        let reloaded = HearthConfig::load_from(&fx.config_path).unwrap();
        assert_eq!(reloaded.default_php, "8.4");
        let _ = sup.stop_service(ServiceKind::PhpFpm);
    }

    #[tokio::test]
    async fn switch_to_launch_blocked_version_removes_stale_fpm() {
        let fx = fixture(false);
        write_fake_fpm_pair(&fx, "8.3");
        write_fake_fpm_pair(&fx, "8.4");
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();

        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        supervisor
            .lock()
            .await
            .register(crate::service::manager::hearth_fpm_service("8.3", &fx.config_dir).unwrap());

        // Launch-block the target: fpm config gone.
        std::fs::remove_file(fx.config_dir.join("fpm/php-fpm.conf")).unwrap();
        let message = switch_php_version(&supervisor, &fx.engine, "8.4")
            .await
            .unwrap();
        assert!(message.contains("php-fpm not registered"), "got: {message}");
        assert!(message.contains("#2343"), "got: {message}");
        assert!(
            supervisor
                .lock()
                .await
                .service(ServiceKind::PhpFpm)
                .is_none(),
            "stale registration must be removed"
        );
    }

    #[tokio::test]
    async fn switch_to_missing_version_errors_before_persisting() {
        let fx = fixture(false);
        fx.config.lock().await.default_php = "8.3".to_string();
        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        let err = switch_php_version(&supervisor, &fx.engine, "8.2")
            .await
            .unwrap_err();
        assert!(err.contains("not installed"), "got: {err}");
        assert_eq!(
            fx.config.lock().await.default_php,
            "8.3",
            "no persist on refusal"
        );
    }

    #[tokio::test]
    async fn switch_aborts_fpm_restart_on_hard_reconcile_failure() {
        let fx = fixture(false);
        write_fake_fpm_pair(&fx, "8.4");
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();
        // Configure a value + a colliding untracked channel file.
        fx.engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        let conf_d = install_fake_homebrew_php(&fx, "8.4");
        std::fs::write(conf_d.join("zz-hearth.ini"), "; not ours\n").unwrap();

        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        let err = switch_php_version(&supervisor, &fx.engine, "8.4")
            .await
            .unwrap_err();
        assert!(err.contains("persisted"), "got: {err}");
        assert!(err.contains("hard failure"), "got: {err}");
        assert!(
            supervisor
                .lock()
                .await
                .service(ServiceKind::PhpFpm)
                .is_none(),
            "no FPM registration/restart after a hard reconcile failure"
        );
    }
}
