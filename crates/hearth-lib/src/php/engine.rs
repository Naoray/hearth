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

        let targets = self.build_targets(&snapshot).await;
        let report = reconcile::reconcile(
            &snapshot,
            &targets,
            &self.manifest_path(),
            &self.provider_roots,
        );
        let file_states = file_states_from_report(&report);
        let files = report
            .files
            .iter()
            .map(|action| FileOutcome {
                path: action.path.to_string_lossy().to_string(),
                result: wire_result(&action.outcome),
            })
            .collect();
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
        let targets = self.build_targets(&snapshot).await;
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

        let targets = self.build_targets(&snapshot).await;
        let report = reconcile::reconcile(
            &snapshot,
            &targets,
            &self.manifest_path(),
            &self.provider_roots,
        );
        let file_states = file_states_from_report(&report);
        let files = report
            .files
            .iter()
            .map(|action| FileOutcome {
                path: action.path.to_string_lossy().to_string(),
                result: wire_result(&action.outcome),
            })
            .collect();
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

    /// Discover + probe + classify every provider target. Hearth-owned conf.d
    /// dirs for configured versions are ensured first (ambient channels must
    /// preexist and are never created — Hearth's own are the one exception).
    async fn build_targets(&self, php_ini: &PhpIniSettings) -> Vec<PhpTarget> {
        let ids = discover_provider_targets(&self.provider_roots);

        for id in &ids {
            if id.provider == PhpProvider::Hearth && !php_ini.effective_for(&id.version).is_empty()
            {
                let conf_d = self
                    .provider_roots
                    .hearth
                    .join("php")
                    .join(&id.version)
                    .join("conf.d");
                let _ = std::fs::create_dir_all(conf_d);
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
        targets
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
}
