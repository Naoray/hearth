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
use crate::php::fpm::{self, FpmConfState};
use crate::php::ini_guard;
use crate::php::reconcile::{
    self, CHANNEL_FILE_NAME, EntryState, LastOutcome, Manifest, WriteOutcome, render_ini,
    sha256_hex,
};
use crate::php::targets::{
    ChannelClass, EffectiveContext, PhpProvider, PhpSapi, PhpTarget, ProbeInfo, ProviderRoots,
    classify_channel, discover_provider_targets, expected_channel_dir, probe_cli_effective,
    probe_fpm_effective, probe_scan_dirs, verify_binary_identity, verify_user_channel,
};
use crate::service::ServiceKind;
use crate::service::supervisor::{FpmOwnership, ServiceSupervisor};
use crate::socket::{
    FileOutcome, FileWriteResult, FpmRestartOutcome, PhpConfigAction, PhpConfigOutcome, PhpScope,
    PhpTargetRow,
};

/// Injected Herd-ownership probe (production: `service::manager::is_herd_running`).
pub type HerdProbe = Arc<dyn Fn() -> FpmOwnership + Send + Sync>;

// Round-4 locked design (review 5594 rev4, brief 5625): the external-FPM
// probe, process-fact gatherer, environment-blob matcher, and evaluator were
// DELETED. Ambient/external FPM observation can never produce verified
// evidence, a managed row, or a write target in Stage B — the entire
// B4-1 class (argv masquerade, mutable titles, ambiguous/duplicate
// executable records, PID reuse, stale evidence) is unrepresentable because
// no ambient authority source exists. Authoritative Hearth-generated FPM
// ownership is deliberately outside the ambient-provider trust boundary.

/// Truthful static coverage for every ambient-provider FPM row in Stage B.
pub const EXTERNAL_FPM_COVERAGE: &str = "unverified: ambient/external FPM launch context cannot be authenticated — Hearth never \
     writes on its behalf and never executes it; use Hearth's supervised FPM for managed/live-observed coverage";

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

    /// Whole-operation deadline used by bounded PHP probes.
    pub fn probe_timeout(&self) -> Duration {
        self.probe_timeout
    }

    pub fn provider_roots(&self) -> &ProviderRoots {
        &self.provider_roots
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.config_dir.join("php").join("manifest.toml")
    }

    /// TYPED current ownership (C4-2) — activation decisions consume this
    /// and fail closed on `Unknown`.
    pub fn fpm_ownership(&self) -> FpmOwnership {
        (self.herd_running)()
    }

    pub fn fpm_conf_state(&self) -> FpmConfState {
        fpm::conf_state(&self.config_dir)
    }

    /// Compatibility adapter for callers that only need the launch-blocked
    /// reason. Typed launch decisions consume [`Self::fpm_conf_state`].
    pub fn fpm_launch_blocked_reason(&self) -> Option<String> {
        match self.fpm_conf_state() {
            FpmConfState::Blocked { reason } => Some(reason),
            FpmConfState::HearthOwned { .. } | FpmConfState::UserManaged { .. } => None,
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
            PhpConfigAction::Show { key } => {
                // C2-4 defense in depth: a socket client's invalid key is a
                // stable actionable error, never a silent valueless report.
                if let Some(k) = &key {
                    ini_guard::validate_key(k).map_err(|e| e.to_string())?;
                }
                self.observe(key.as_deref()).await
            }
            PhpConfigAction::Status { key, token } => {
                if let Some(k) = &key {
                    ini_guard::validate_key(k).map_err(|e| e.to_string())?;
                }
                let mut outcome = self.observe(key.as_deref()).await?;
                // C2-2/C3-2: certify the keyed-Status capability by echoing
                // BOTH the key this engine actually applied and the caller's
                // per-request correlation token. A legacy daemon ignores the
                // fields and can echo neither — clients treat any missing or
                // mismatched echo as a version mismatch, and a stale report
                // (right key, wrong request) fails the token binding.
                outcome.status_key = key;
                outcome.status_token = token;
                Ok(outcome)
            }
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
        let mut rows = self.build_rows(&snapshot, &default_php, &targets, Some(key), &file_states);
        self.fill_launch_probes(&mut rows, &targets, key, &default_php)
            .await;

        Ok(PhpConfigOutcome {
            persisted: Some(true),
            rows,
            files,
            fpm: FpmRestartOutcome::NotAttempted,
            status_key: None,
            status_token: None,
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
        let mut rows = self.build_rows(&snapshot, &default_php, &targets, key, &file_states);
        if let Some(key) = key {
            self.fill_launch_probes(&mut rows, &targets, key, &default_php)
                .await;
        }
        Ok(PhpConfigOutcome {
            persisted: None,
            rows,
            files: Vec::new(),
            fpm: FpmRestartOutcome::NotAttempted,
            status_key: None,
            status_token: None,
        })
    }

    /// Journal recovery → guarded legacy migration → reconcile.
    async fn sync_locked(&self) -> Result<PhpConfigOutcome, String> {
        self.sync_locked_mode(true).await
    }

    async fn sync_locked_mode(
        &self,
        materialize_static_fpm: bool,
    ) -> Result<PhpConfigOutcome, String> {
        let manifest_dir = self.config_dir.join("php");
        crate::php::targets::ensure_hearth_channel_dir(&self.provider_roots.hearth, &manifest_dir)
            .map_err(|error| format!("manifest directory guard failed: {error}"))?;
        reconcile::recover_pending(&self.manifest_path(), &self.provider_roots.hearth)
            .map_err(|e| format!("journal recovery failed: {e}"))?;
        if materialize_static_fpm {
            fpm::recover_pending_fpm(&self.config_dir, &self.provider_roots.hearth)
                .map_err(|e| format!("FPM journal recovery failed: {e}"))?;
        }

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
        if materialize_static_fpm {
            let fpm_report = fpm::materialize(&self.config_dir, &self.provider_roots.hearth);
            if !matches!(fpm_report.state, FpmConfState::UserManaged { .. }) {
                files.extend([
                    FileOutcome {
                        path: fpm::conf_path(&self.config_dir)
                            .to_string_lossy()
                            .to_string(),
                        result: wire_result(&fpm_report.conf),
                    },
                    FileOutcome {
                        path: fpm::probe_script_path(&self.config_dir)
                            .to_string_lossy()
                            .to_string(),
                        result: wire_result(&fpm_report.probe),
                    },
                ]);
            }
        }
        let rows = self.build_rows(&snapshot, &default_php, &targets, None, &file_states);

        Ok(PhpConfigOutcome {
            persisted: None,
            rows,
            files,
            fpm: FpmRestartOutcome::NotAttempted,
            status_key: None,
            status_token: None,
        })
    }

    /// Remove every manifest-tracked channel file (journal-resolved,
    /// exact-hash only — survivors are reported Refused).
    fn unmanage_locked(&self) -> Result<PhpConfigOutcome, String> {
        let mut actions = reconcile::unmanage(&self.manifest_path(), &self.provider_roots.hearth)
            .map_err(|e| e.to_string())?;
        actions.extend(
            fpm::unmanage_fpm(&self.config_dir, &self.provider_roots.hearth)
                .map_err(|e| e.to_string())?,
        );
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
            status_key: None,
            status_token: None,
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
            // C3-3 (review 5650 r3): provider identity must be PROVEN before
            // any use. An expected layout path whose canonical target
            // escapes its canonical provider root is NEVER executed nor
            // granted a channel under that label — it renders as a loudly
            // unverified target instead of being silently skipped (the
            // typed Hearth channel-creation refusal above still fires for
            // symlinked ancestors, preserving Stage-B loudness).
            if let Err(reason) =
                verify_binary_identity(id.provider, &id.version, id.sapi, &self.provider_roots)
            {
                let reason = format!("target identity unverified: {reason}");
                targets.push(PhpTarget {
                    id,
                    probe: ProbeInfo::default(),
                    normal_channel: ChannelClass::BestEffort {
                        reason: reason.clone(),
                    },
                    sanitized_channel: ChannelClass::BestEffort { reason },
                    write_channel: None,
                    identity_verified: false,
                });
                continue;
            }

            // C1-2 (review 5650): NO FPM binary is EVER executed for
            // classification — `-i` runs only as the launch probe of the
            // registrable service under its exact service env. External FPM
            // identity is known from the provider layout alone (Round-4:
            // ambient observation can grant nothing anyway), and the Hearth
            // FPM write channel is the deterministic verified Hearth conf.d
            // — no execution required for either.
            if id.sapi == PhpSapi::Fpm {
                let is_external_fpm = id.provider != PhpProvider::Hearth;
                let reason = "fpm binaries are never executed for classification — \
                              FPM runs `-i` only as the service launch probe";
                let write_channel = if is_external_fpm {
                    None
                } else {
                    let dir = expected_channel_dir(id.provider, &id.version, &self.provider_roots);
                    verify_user_channel(&dir, &self.provider_roots.allowed_prefixes())
                        .ok()
                        .map(|_| dir)
                };
                targets.push(PhpTarget {
                    id,
                    probe: ProbeInfo::default(),
                    normal_channel: ChannelClass::BestEffort {
                        reason: reason.to_string(),
                    },
                    sanitized_channel: ChannelClass::BestEffort {
                        reason: reason.to_string(),
                    },
                    write_channel,
                    identity_verified: true,
                });
                continue;
            }

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
                identity_verified: true,
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

    // (Round-4: the former external-evidence exactness gate is gone with the
    // evidence type itself — no ambient observation reaches write authority.)

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

            // Round-4 locked design: ambient (non-Hearth) FPM targets get
            // exactly ONE static, truthful row. No ambient observation can
            // prove or manage them in Stage B, so the coverage text is a
            // constant and the channel is always absent — see
            // EXTERNAL_FPM_COVERAGE; ambient ownership is never inferred.
            if target.id.provider != PhpProvider::Hearth && target.id.sapi == PhpSapi::Fpm {
                rows.push(PhpTargetRow {
                    provider: provider_str(target.id.provider).to_string(),
                    version: target.id.version.clone(),
                    sapi: sapi_str(target.id.sapi).to_string(),
                    context: "external".to_string(),
                    channel: None,
                    coverage: EXTERNAL_FPM_COVERAGE.to_string(),
                    configured: configured.clone(),
                    observed: None,
                    observed_state: "n/a".to_string(),
                    pending_restart: false,
                    run_state: None,
                });
                continue;
            }

            // C1-2: Hearth's own FPM has exactly ONE launch surface — the
            // supervised service row appended below. Terminal-context rows
            // would imply classification coverage FPM never gets (its
            // binaries are never executed outside the service launch probe).
            if target.id.sapi == PhpSapi::Fpm {
                continue;
            }

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
                    pending_restart: false,
                    run_state: None,
                });
            }
        }

        // Hearth's own supervised FPM service row — exhaustive on TYPED
        // ownership (C5-1C): Owned = Herd's domain, no Hearth row; Unowned =
        // truthful supervised/LAUNCH-BLOCKED row; Unknown = a LOUD
        // ownership-unknown row with the diagnostic + remediation — never
        // rendered as if ownership were proven Unowned.
        match self.fpm_ownership() {
            FpmOwnership::Owned => {}
            FpmOwnership::Unowned => {
                let effective = php_ini.effective_for(default_php);
                let binary_missing =
                    crate::php::resolver::resolve_phpfpm_binary(default_php, &self.config_dir)
                        .is_none();
                let coverage = match self.fpm_conf_state() {
                    FpmConfState::Blocked { reason } => format!("LAUNCH-BLOCKED: {reason}"),
                    FpmConfState::HearthOwned { .. } | FpmConfState::UserManaged { .. }
                        if binary_missing =>
                    {
                        format!("LAUNCH-BLOCKED: php-fpm binary for {default_php} not found")
                    }
                    FpmConfState::HearthOwned { listen, .. } => format!(
                        "supervised (scan-dir env at launch; listen: {})",
                        listen.display()
                    ),
                    FpmConfState::UserManaged { .. } => "supervised (user-managed fpm config — \
                         live observation requires Hearth-generated config; remove/rename it and \
                         run --sync to adopt)"
                        .to_string(),
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
                    pending_restart: false,
                    run_state: None,
                });
            }
            FpmOwnership::Unknown(diag) => {
                let effective = php_ini.effective_for(default_php);
                rows.push(PhpTargetRow {
                    provider: "hearth".to_string(),
                    version: default_php.to_string(),
                    sapi: "fpm".to_string(),
                    context: "launched".to_string(),
                    channel: None,
                    coverage: format!(
                        "OWNERSHIP-UNKNOWN: {diag} — FPM activation fails closed; fix the \
                         ownership probe and retry"
                    ),
                    configured: key.and_then(|k| effective.get(k).cloned()),
                    observed: None,
                    observed_state: "n/a".to_string(),
                    pending_restart: false,
                    run_state: None,
                });
            }
        }
        rows
    }

    /// Task 8: launch-probed effective values for freshly built rows.
    ///
    /// CLI targets run `-r 'echo ini_get("<key>");'` under the EXACT
    /// classification-context env (normal / sanitized). Only the
    /// Hearth-supervised, registrable FPM service row is probed — `-i`
    /// under the exact service env (launch-probed, never live-observed;
    /// live FastCGI observation is a separate evidence-gated stage). Ambient external FPM rows
    /// are never probed: no ambient launch context can be authenticated
    /// (Stage-B locked design), so their static unverified row stands.
    /// A probe failure leaves the row's truthful non-success state
    /// untouched and can never fail the surrounding command.
    async fn fill_launch_probes(
        &self,
        rows: &mut [PhpTargetRow],
        targets: &[PhpTarget],
        key: &str,
        default_php: &str,
    ) {
        // Defense in depth: mutating actions already validated the key; Show
        // passes user input straight here. An invalid key is never embedded.
        if ini_guard::validate_key(key).is_err() {
            return;
        }
        for target in targets {
            // C3-3: an unverified identity is never executed — not even for
            // an effective-value probe.
            if target.id.sapi != PhpSapi::Cli || !target.identity_verified {
                continue;
            }
            for (context, effective_context) in [
                ("normal", EffectiveContext::Normal),
                ("sanitized", EffectiveContext::Sanitized),
            ] {
                let probed = probe_cli_effective(
                    &target.id.binary,
                    key,
                    effective_context,
                    self.probe_timeout,
                )
                .await;
                let Ok(Some(value)) = probed else {
                    continue;
                };
                let provider = provider_str(target.id.provider);
                if let Some(row) = rows.iter_mut().find(|r| {
                    r.provider == provider
                        && r.version == target.id.version
                        && r.sapi == "cli"
                        && r.context == context
                }) {
                    row.observed = Some(value);
                    row.observed_state = "launch-probed".to_string();
                }
            }
        }

        // The supervised FPM service row (context `launched`), only while
        // registrable under PROVEN Unowned ownership (C5-1C exhaustive):
        // Owned AND Unknown both mean zero `-i` execution.
        match self.fpm_ownership() {
            FpmOwnership::Unowned => {}
            FpmOwnership::Owned | FpmOwnership::Unknown(_) => return,
        }
        if self.fpm_launch_blocked_reason().is_some() {
            return;
        }
        let Some(binary) =
            crate::php::resolver::resolve_phpfpm_binary(default_php, &self.config_dir)
        else {
            return;
        };
        let service_env = vec![crate::php::scan_dir_env(&self.config_dir, default_php)];
        if let Ok(Some(value)) =
            probe_fpm_effective(&binary, key, &service_env, self.probe_timeout).await
            && let Some(row) = rows
                .iter_mut()
                .find(|r| r.sapi == "fpm" && r.context == "launched")
        {
            row.observed = Some(value);
            row.observed_state = "launch-probed".to_string();
        }
    }
}

/// Restart the supervised php-fpm only when it is actually registered.
/// Unregistered/optional FPM is informational (`NotRegistered`/`LaunchBlocked`),
/// never a command failure; a registered restart failure stays a hard `Failed`.
///
/// C2-1 (review 5650 r2): this is THE centralized FPM-restart policy for
/// every config mutation entry point — CLI/daemon Set & Unset and the MCP
/// config write all route here — so the Herd-coexistence decision cannot
/// drift between surfaces. The ownership check runs FIRST, before any
/// supervisor access: under live Herd, persistence/reconciliation have
/// already happened safely, and the FPM supervisor state (registered or
/// not, running or not) is left byte-for-byte untouched.
pub async fn restart_fpm_conditionally(
    supervisor: &Arc<Mutex<ServiceSupervisor>>,
    engine: &PhpConfigEngine,
) -> FpmRestartOutcome {
    match engine.fpm_ownership() {
        FpmOwnership::Owned => {
            // Read-only registration peek — truthful wording either way,
            // zero stop/start/register/remove.
            let registered = supervisor
                .lock()
                .await
                .status()
                .contains_key(&ServiceKind::PhpFpm);
            return if registered {
                FpmRestartOutcome::SkippedHerdOwned
            } else {
                FpmRestartOutcome::NotRegistered { herd_hint: true }
            };
        }
        // C4-2: unknown evidence fails CLOSED — never a restart, and the
        // report says WHY instead of pretending Herd absent.
        FpmOwnership::Unknown(diag) => {
            return FpmRestartOutcome::SkippedOwnershipUnknown { reason: diag };
        }
        FpmOwnership::Unowned => {}
    }
    let mut sup = supervisor.lock().await;
    if !sup.status().contains_key(&ServiceKind::PhpFpm) {
        drop(sup);
        if let Some(reason) = engine.fpm_launch_blocked_reason() {
            return FpmRestartOutcome::LaunchBlocked { reason };
        }
        return FpmRestartOutcome::NotRegistered { herd_hint: false };
    }
    // C7-1 (review 5650 r8): the mutation itself is the supervisor's ONE
    // atomic ownership-snapshot transaction — the engine reading above is a
    // read-only fast path only. An Unowned→Owned/Unknown flip landing after
    // that reading is refused HERE with zero mutation; the old split
    // stop-then-guarded-start could stop the child before refusing.
    use crate::service::supervisor::FpmRestartTxOutcome;
    match sup.restart_fpm_service() {
        FpmRestartTxOutcome::Restarted => FpmRestartOutcome::Restarted,
        FpmRestartTxOutcome::SkippedOwned => FpmRestartOutcome::SkippedHerdOwned,
        FpmRestartTxOutcome::SkippedOwnershipUnknown(diag) => {
            FpmRestartOutcome::SkippedOwnershipUnknown { reason: diag }
        }
        FpmRestartTxOutcome::NotRegistered => FpmRestartOutcome::NotRegistered { herd_hint: false },
        FpmRestartTxOutcome::StopFailed { error } => FpmRestartOutcome::Failed {
            message: format!("php-fpm stop failed: {error}"),
        },
        FpmRestartTxOutcome::StartFailed { error } => FpmRestartOutcome::Failed {
            message: format!("php-fpm start failed: {error}"),
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
    engine.switch(supervisor, version).await
}

impl PhpConfigEngine {
    /// First-class serialized version switch (review 5594 B2-3): ONE
    /// `op_lock` acquisition covers validation, `default_php` persistence,
    /// reconciliation (via the private, already-locked sync path — never a
    /// recursive lock), the hard-failure gate, AND the FPM service
    /// replacement. Concurrent Set/Unset/Sync/Switch therefore cannot
    /// interleave between the persisted default and its reconcile, and an
    /// older switch can never overwrite the service after a newer committed
    /// one — supervisor mutation happens inside the same critical section,
    /// after config/probe work, preserving the project lock order
    /// (`config → … → supervisor`; both are async mutexes, so awaiting them
    /// under the async op lock is sound).
    pub async fn switch(
        &self,
        supervisor: &Arc<Mutex<ServiceSupervisor>>,
        version: &str,
    ) -> Result<String, String> {
        let _op = self.op_lock.lock().await;
        validate_version(version)?;
        let config_dir = self.config_dir.clone();
        if crate::php::resolver::resolve_php_binary(version, &config_dir).is_none()
            && crate::php::resolver::resolve_phpfpm_binary(version, &config_dir).is_none()
        {
            return Err(format!(
                "PHP {version} is not installed (no CLI or FPM binary found)"
            ));
        }

        {
            let mut cfg = self.config.lock().await;
            cfg.default_php = version.to_string();
            cfg.save_to(&self.config_path)
                .map_err(|e| format!("failed to save config: {e}"))?;
        }
        // Reconcile for the new active version, hard-gated: a Refused/Failed
        // channel aborts the FPM restart (config release above keeps lock
        // order; op lock stays held for the whole transaction).
        require_reconciled(self.sync_locked_mode(false).await)
            .map_err(|e| format!("switched default_php to {version} (persisted), but: {e}"))?;

        // C6-2 (review 5650 r7): the ENTIRE FPM replacement is one
        // supervisor-owned transaction under ONE authoritative typed
        // ownership snapshot taken immediately before mutation. No engine-
        // level pre-checks remain — split checks created windows where a
        // flip could refuse AFTER destructive mutation while reporting
        // "untouched". Preparation happens before the old child is touched;
        // an ownership change after the snapshot is the health handoff's
        // job. Version persistence + reconcile above stand regardless.
        use crate::service::supervisor::FpmReplaceOutcome;
        let mut sup = supervisor.lock().await;
        let build_version = version.to_string();
        let build_config_dir = config_dir.clone();
        match sup.replace_fpm_service(move || {
            crate::service::manager::hearth_fpm_service(&build_version, &build_config_dir)
        }) {
            FpmReplaceOutcome::Replaced => {
                Ok(format!("Switched to PHP {version}; php-fpm restarted"))
            }
            FpmReplaceOutcome::SkippedOwned => Ok(format!(
                "Switched to PHP {version}; php-fpm untouched (Herd manages PHP-FPM)"
            )),
            FpmReplaceOutcome::SkippedOwnershipUnknown(diag) => Ok(format!(
                "Switched to PHP {version}; php-fpm untouched (Herd ownership \
                 unknown: {diag} — FPM activation skipped fail-closed)"
            )),
            FpmReplaceOutcome::BuildFailed {
                reason,
                removed_stale,
            } => {
                let stale_note = if removed_stale {
                    "; previous php-fpm registration removed"
                } else {
                    ""
                };
                Ok(format!(
                    "Switched to PHP {version}; php-fpm not registered: {reason}{stale_note}"
                ))
            }
            FpmReplaceOutcome::StopFailed { error } => Err(format!(
                "switched to PHP {version} (persisted), but stopping the previous php-fpm \
                 failed: {error} — the previous php-fpm remains supervised for retry"
            )),
            FpmReplaceOutcome::StartFailed { error } => Err(format!(
                "switched to PHP {version}, but php-fpm start failed: {error}"
            )),
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
        fixture_with_probe(Arc::new(move || {
            if herd {
                FpmOwnership::Owned
            } else {
                FpmOwnership::Unowned
            }
        }))
    }

    /// Flippable-ownership fixture (C2-1): the probe reads live state, so a
    /// test can flip Herd ownership AFTER registering/starting FPM.
    fn fixture_with_herd_flag() -> (Fixture, Arc<std::sync::atomic::AtomicBool>) {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_flag = Arc::clone(&flag);
        let fx = fixture_with_probe(Arc::new(move || {
            if probe_flag.load(std::sync::atomic::Ordering::SeqCst) {
                FpmOwnership::Owned
            } else {
                FpmOwnership::Unowned
            }
        }));
        (fx, flag)
    }

    fn fixture_with_probe(herd: HerdProbe) -> Fixture {
        // Canonicalized base — /var → /private/var on macOS would otherwise
        // break allowlist prefix checks.
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let config_dir = base.join("hearth config"); // path with a space
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = base.join("config.toml");
        let roots = ProviderRoots::isolated(
            &base,
            config_dir.clone(),
            base.join("herd"),
            base.join("homebrew"),
        )
        .unwrap();
        let config = Arc::new(Mutex::new(HearthConfig::default()));
        let engine = Arc::new(PhpConfigEngine::new(
            Arc::clone(&config),
            config_path.clone(),
            config_dir.clone(),
            roots,
            herd,
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
                    reason.contains("run `hearth php config --sync`"),
                    "{reason}"
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
    async fn status_reports_hearth_fpm_launch_blocked_with_sync_remediation() {
        let fx = fixture(false);
        let outcome = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
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
            fpm_row.coverage.contains("run `hearth php config --sync`"),
            "got: {}",
            fpm_row.coverage
        );
    }

    #[tokio::test]
    async fn pre_reconcile_recovery_rejects_symlinked_ini_manifest_dir() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let fx = fixture(false);
        let outside = fx.config_dir.parent().unwrap().join("outside-ini-recovery");
        std::fs::create_dir_all(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, b"pre-recovery-outside").unwrap();
        std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = std::fs::metadata(&sentinel).unwrap();
        std::os::unix::fs::symlink(&outside, fx.config_dir.join("php")).unwrap();

        let error = fx.engine.boot_sync().await.unwrap_err();
        assert!(error.contains("manifest directory guard failed"), "{error}");
        let after = std::fs::metadata(&sentinel).unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"pre-recovery-outside");
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.permissions().mode() & 0o777, 0o640);
        assert!(!outside.join(".manifest.lock").exists());
        assert!(!outside.join("manifest.toml").exists());
    }

    #[tokio::test]
    async fn boot_sync_generates_fpm_conf_and_service_registers() {
        let fx = fixture(false);
        write_fake_fpm_pair(&fx, "8.4");

        let outcome = fx.engine.boot_sync().await.unwrap();
        assert_eq!(outcome.fpm, FpmRestartOutcome::NotAttempted);
        assert!(matches!(
            fx.engine.fpm_conf_state(),
            crate::php::fpm::FpmConfState::HearthOwned { .. }
        ));
        let service = crate::service::manager::hearth_fpm_service("8.4", &fx.config_dir)
            .expect("boot sync makes FPM launchable");
        assert!(matches!(
            service.fpm_launch_conf(),
            Some(crate::service::supervisor::FpmLaunchConf::HearthOwned { .. })
        ));
    }

    #[tokio::test]
    async fn status_row_shows_listener_for_hearth_owned_conf() {
        let fx = fixture(false);
        fx.engine.boot_sync().await.unwrap();

        let outcome = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
        let row = outcome
            .rows
            .iter()
            .find(|row| row.provider == "hearth" && row.sapi == "fpm")
            .unwrap();
        assert_eq!(
            row.coverage,
            format!(
                "supervised (scan-dir env at launch; listen: {})",
                fx.config_dir.join("run/php-fpm.sock").display()
            )
        );
    }

    #[tokio::test]
    async fn user_managed_conf_row_is_truthful_and_launchable() {
        let fx = fixture(false);
        write_fake_fpm_pair(&fx, "8.4");
        let conf = fx.config_dir.join("fpm/php-fpm.conf");
        std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
        std::fs::write(&conf, "; foreign\n").unwrap();

        let status = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
        let row = status
            .rows
            .iter()
            .find(|row| row.provider == "hearth" && row.sapi == "fpm")
            .unwrap();
        assert_eq!(
            row.coverage,
            "supervised (user-managed fpm config — live observation requires Hearth-generated \
             config; remove/rename it and run --sync to adopt)"
        );
        let service = crate::service::manager::hearth_fpm_service("8.4", &fx.config_dir).unwrap();
        assert_eq!(
            service.fpm_launch_conf(),
            Some(&crate::service::supervisor::FpmLaunchConf::UserManaged { conf })
        );
    }

    #[tokio::test]
    async fn blocked_generation_renders_launch_blocked_with_remediation() {
        use std::os::unix::fs::PermissionsExt;

        let fx = fixture(false);
        fx.engine.boot_sync().await.unwrap();
        fx.engine.apply(PhpConfigAction::Unmanage).await.unwrap();
        let fpm_dir = fx.config_dir.join("fpm");
        std::fs::set_permissions(&fpm_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let sync = fx.engine.apply(PhpConfigAction::Sync).await.unwrap();
        let row = sync
            .rows
            .iter()
            .find(|row| row.provider == "hearth" && row.sapi == "fpm")
            .unwrap();
        assert!(row.coverage.starts_with("LAUNCH-BLOCKED:"), "{row:?}");
        assert!(row.coverage.contains("--sync"), "{row:?}");

        let set = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        std::fs::set_permissions(&fpm_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            set.persisted,
            Some(true),
            "Set stays independent of static FPM generation"
        );
        assert!(!fpm::conf_path(&fx.config_dir).exists());
    }

    #[tokio::test]
    async fn sync_under_owned_or_unknown_writes_files_zero_supervisor_mutation() {
        for ownership in [
            FpmOwnership::Owned,
            FpmOwnership::Unknown("probe unavailable".to_string()),
        ] {
            let fx = fixture_with_probe(Arc::new(move || ownership.clone()));
            let supervisor = ServiceSupervisor::new();
            let before = supervisor.status();
            let outcome = fx.engine.apply(PhpConfigAction::Sync).await.unwrap();
            let after = supervisor.status();
            assert!(before.is_empty() && after.is_empty());
            assert!(matches!(
                fx.engine.fpm_conf_state(),
                crate::php::fpm::FpmConfState::HearthOwned { .. }
            ));
            assert!(
                outcome
                    .files
                    .iter()
                    .any(|file| file.path.ends_with("php-fpm.conf"))
            );
            assert_eq!(outcome.fpm, FpmRestartOutcome::NotAttempted);
        }
    }

    #[tokio::test]
    async fn sync_never_registers_or_restarts_fpm() {
        let fx = fixture(false);
        let supervisor = supervisor_unowned();
        let outcome = fx.engine.apply(PhpConfigAction::Sync).await.unwrap();

        assert!(
            supervisor
                .lock()
                .await
                .service(ServiceKind::PhpFpm)
                .is_none()
        );
        assert_eq!(outcome.fpm, FpmRestartOutcome::NotAttempted);
        assert!(matches!(
            fx.engine.fpm_conf_state(),
            crate::php::fpm::FpmConfState::HearthOwned { .. }
        ));
    }

    #[tokio::test]
    async fn unmanage_while_running_keeps_child_blocks_future_start() {
        let fx = fixture(false);
        write_fake_fpm_pair(&fx, "8.4");
        fx.engine.boot_sync().await.unwrap();
        let supervisor = supervisor_unowned();
        {
            let mut sup = supervisor.lock().await;
            sup.register(
                crate::service::manager::hearth_fpm_service("8.4", &fx.config_dir).unwrap(),
            )
            .unwrap();
            sup.start_service(ServiceKind::PhpFpm).unwrap();
        }
        let before = supervisor.lock().await.fpm_launch_snapshot().unwrap();

        fx.engine.apply(PhpConfigAction::Unmanage).await.unwrap();

        let after = supervisor.lock().await.fpm_launch_snapshot().unwrap();
        assert_eq!(after, before, "unmanage must not touch the running child");
        assert!(matches!(
            fx.engine.fpm_conf_state(),
            crate::php::fpm::FpmConfState::Blocked { .. }
        ));
        assert!(crate::service::manager::hearth_fpm_service("8.4", &fx.config_dir).is_err());
        let row = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap()
            .rows
            .into_iter()
            .find(|row| row.provider == "hearth" && row.sapi == "fpm")
            .unwrap();
        assert!(row.coverage.starts_with("LAUNCH-BLOCKED:"), "{row:?}");
        supervisor
            .lock()
            .await
            .stop_service(ServiceKind::PhpFpm)
            .unwrap();

        let stopped = fixture(false);
        write_fake_fpm_pair(&stopped, "8.4");
        stopped.engine.boot_sync().await.unwrap();
        let stopped_supervisor = supervisor_unowned();
        stopped_supervisor
            .lock()
            .await
            .register(
                crate::service::manager::hearth_fpm_service("8.4", &stopped.config_dir).unwrap(),
            )
            .unwrap();
        stopped
            .engine
            .apply(PhpConfigAction::Unmanage)
            .await
            .unwrap();
        assert!(
            stopped_supervisor
                .lock()
                .await
                .service(ServiceKind::PhpFpm)
                .is_some(),
            "stopped registration is also untouched"
        );
        assert!(
            crate::service::manager::hearth_fpm_service("8.4", &stopped.config_dir).is_err(),
            "future service construction is blocked"
        );
    }

    #[tokio::test]
    async fn status_under_herd_has_no_hearth_fpm_service_row() {
        let fx = fixture(true);
        let outcome = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
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
                reason: "missing fpm config — run `hearth php config --sync`".to_string(),
            },
            status_key: None,
            status_token: None,
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
            status_key: None,
            status_token: None,
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
            status_key: None,
            status_token: None,
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

        let outcome = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
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

    /// C6-1: supervisor carrying explicit isolated Unowned probe evidence —
    /// probe-less supervisors refuse every FPM mutation by construction.
    fn supervisor_unowned() -> Arc<Mutex<ServiceSupervisor>> {
        let mut sup = ServiceSupervisor::new();
        sup.set_fpm_ownership_probe(Arc::new(|| {
            crate::service::supervisor::FpmOwnership::Unowned
        }));
        Arc::new(Mutex::new(sup))
    }

    fn write_fake_fpm_pair(fx: &Fixture, version: &str) {
        // Long-running fake so start_service succeeds and supervision works.
        write_executable(
            &fx.config_dir.join("php").join(version).join("php-fpm"),
            "#!/bin/sh\n[ \"$1\" = \"--warmup\" ] && exit 0\nsleep 30\n",
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

        let supervisor = supervisor_unowned();
        {
            // Old registration carrying the OLD version's env.
            let mut sup = supervisor.lock().await;
            sup.register(
                crate::service::manager::hearth_fpm_service("8.3", &fx.config_dir).unwrap(),
            )
            .unwrap();
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

        let supervisor = supervisor_unowned();
        supervisor
            .lock()
            .await
            .register(crate::service::manager::hearth_fpm_service("8.3", &fx.config_dir).unwrap())
            .unwrap();

        // Static FPM generation is reserved for boot/explicit Sync.
        std::fs::remove_file(fx.config_dir.join("fpm/php-fpm.conf")).unwrap();
        let message = switch_php_version(&supervisor, &fx.engine, "8.4")
            .await
            .unwrap();
        assert!(message.contains("php-fpm not registered"), "got: {message}");
        assert!(
            message.contains("run `hearth php config --sync`"),
            "got: {message}"
        );
        assert!(
            supervisor
                .lock()
                .await
                .service(ServiceKind::PhpFpm)
                .is_none(),
            "stale registration must be removed"
        );
        assert!(matches!(
            fx.engine.fpm_conf_state(),
            FpmConfState::Blocked { .. }
        ));
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

    /// Fake Herd-style provider: CLI+FPM binaries whose DAEMON-ENV probes
    /// report the user-scoped channel (the B2-1 hazard scenario: inherited
    /// launcher vars make the daemon see a verified channel).
    fn install_fake_herd_pair(fx: &Fixture, version: &str) -> PathBuf {
        let herd = fx.engine.provider_roots.herd.clone();
        let xy = version.replace('.', "");
        let user_channel = herd.join("config/php").join(&xy);
        std::fs::create_dir_all(&user_channel).unwrap();
        let script = format!(
            "#!/bin/sh\necho \"Scan for additional .ini files in: {}\"\n",
            user_channel.display()
        );
        write_executable(&herd.join("bin").join(format!("php{xy}")), &script);
        write_executable(&herd.join("bin").join(format!("php{xy}-fpm")), &script);
        user_channel
    }

    #[tokio::test]
    async fn external_fpm_row_is_static_and_cli_rows_stay_independent() {
        // Round-4 locked design: the ambient herd FPM row is a constant,
        // truthful "unverified" — even while the CLI twin's own probes prove
        // a managed channel. The daemon env is never external evidence.
        let fx = fixture(false);
        install_fake_herd_pair(&fx, "8.4");
        fx.engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();

        let outcome = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
        let fpm_rows: Vec<_> = outcome
            .rows
            .iter()
            .filter(|r| r.provider == "herd" && r.sapi == "fpm")
            .collect();
        assert_eq!(fpm_rows.len(), 1, "exactly one external row: {fpm_rows:?}");
        assert_eq!(fpm_rows[0].context, "external");
        assert_eq!(
            fpm_rows[0].coverage, EXTERNAL_FPM_COVERAGE,
            "external FPM coverage is a static constant — no ambient observation"
        );
        assert!(
            fpm_rows[0].channel.is_none(),
            "external row never carries a channel"
        );
        // CLI context evidence stays separate: normal row is managed.
        let cli_normal = outcome
            .rows
            .iter()
            .find(|r| r.provider == "herd" && r.sapi == "cli" && r.context == "normal")
            .expect("herd cli normal row");
        assert!(
            cli_normal.coverage.starts_with("managed"),
            "got: {}",
            cli_normal.coverage
        );
    }

    #[tokio::test]
    async fn external_fpm_never_grants_write_authority_even_with_perfect_evidence() {
        // Round-4 locked design: NO ambient external-FPM observation may
        // authorize a write or a managed row in Stage B. Fixture has ONLY
        // the FPM binary (no CLI twin), so any channel file could come from
        // external FPM authority alone — and none may exist.
        let fx = fixture(false);
        let herd = fx.engine.provider_roots.herd.clone();
        let user_channel = herd.join("config/php/84");
        std::fs::create_dir_all(&user_channel).unwrap();
        write_executable(
            &herd.join("bin/php84-fpm"),
            &format!(
                "#!/bin/sh\necho \"Scan for additional .ini files in: {}\"\n",
                user_channel.display()
            ),
        );

        fx.engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert!(
            !user_channel.join("zz-hearth.ini").exists(),
            "ambient external FPM must NEVER be a write target in Stage B"
        );

        let status = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
        let fpm_row = status
            .rows
            .iter()
            .find(|r| r.provider == "herd" && r.sapi == "fpm")
            .expect("external fpm row");
        assert!(
            fpm_row.coverage.starts_with("unverified"),
            "external row must be truthfully unverified, got: {}",
            fpm_row.coverage
        );
        assert!(
            !fpm_row.coverage.contains("privileged-dir"),
            "no privileged-dir claim without observation"
        );
        assert!(fpm_row.coverage.contains("never executes it"));
        assert!(fpm_row.coverage.contains("Hearth's supervised FPM"));
    }

    #[tokio::test]
    async fn switch_serializes_with_set_under_one_op_lock() {
        let fx = fixture(false);
        write_fake_fpm_pair(&fx, "8.3");
        write_fake_fpm_pair(&fx, "8.4");
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();
        fx.config.lock().await.default_php = "8.3".to_string();
        let supervisor = supervisor_unowned();

        // Deterministic barrier: hold the engine op lock; both operations
        // must block behind it (no sleeps deciding the outcome).
        let barrier = fx.engine.op_lock.lock().await;
        let e1 = Arc::clone(&fx.engine);
        let sup1 = Arc::clone(&supervisor);
        let switch_task = tokio::spawn(async move { e1.switch(&sup1, "8.4").await });
        let e2 = Arc::clone(&fx.engine);
        let set_task =
            tokio::spawn(async move { e2.apply(set_global("memory_limit", "1G")).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            !switch_task.is_finished(),
            "switch must wait on the op lock"
        );
        assert!(!set_task.is_finished(), "set must wait on the op lock");

        drop(barrier);
        switch_task.await.unwrap().unwrap();
        set_task.await.unwrap().unwrap();

        // Invariant: registered FPM service matches the committed default.
        let cfg_default = fx.config.lock().await.default_php.clone();
        assert_eq!(cfg_default, "8.4");
        let mut sup = supervisor.lock().await;
        let svc = sup.service(ServiceKind::PhpFpm).expect("fpm registered");
        assert!(svc.env()[0].1.contains("8.4"), "env: {:?}", svc.env());
        let _ = sup.stop_service(ServiceKind::PhpFpm);
    }

    #[tokio::test]
    async fn two_concurrent_switches_leave_service_consistent_with_committed_default() {
        let fx = fixture(false);
        write_fake_fpm_pair(&fx, "8.3");
        write_fake_fpm_pair(&fx, "8.4");
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();
        let supervisor = supervisor_unowned();

        let barrier = fx.engine.op_lock.lock().await;
        let e1 = Arc::clone(&fx.engine);
        let sup1 = Arc::clone(&supervisor);
        let t1 = tokio::spawn(async move { e1.switch(&sup1, "8.3").await });
        let e2 = Arc::clone(&fx.engine);
        let sup2 = Arc::clone(&supervisor);
        let t2 = tokio::spawn(async move { e2.switch(&sup2, "8.4").await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!t1.is_finished() && !t2.is_finished());
        drop(barrier);
        t1.await.unwrap().unwrap();
        t2.await.unwrap().unwrap();

        // Whichever switch committed LAST owns both the default and the
        // service — an older switch can never overwrite a newer one.
        let cfg_default = fx.config.lock().await.default_php.clone();
        let mut sup = supervisor.lock().await;
        let svc = sup.service(ServiceKind::PhpFpm).expect("fpm registered");
        assert!(
            svc.command()
                .contains(&format!("php/{cfg_default}/php-fpm")),
            "service binary {} must match committed default {cfg_default}",
            svc.command()
        );
        assert!(
            svc.env()[0].1.contains(&cfg_default),
            "service env {:?} must match committed default {cfg_default}",
            svc.env()
        );
        let _ = sup.stop_service(ServiceKind::PhpFpm);
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

    // ---- Task 8: launch-probed observation ----

    /// Fake Homebrew PHP that answers classification probes with a Verified
    /// channel AND the effective-value probe (`-r`) with a per-context value
    /// (HOME present = normal env, cleared = sanitized).
    fn install_fake_homebrew_php_with_values(fx: &Fixture, version: &str) -> PathBuf {
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
                r#"#!/bin/sh
if [ "$1" = "--warmup" ]; then exit 0; fi
if [ "$1" = "-r" ]; then
    [ "$2" = 'echo ini_get("memory_limit");' ] || exit 9
    if [ -n "$HOME" ]; then printf '1G'; else printf '777M'; fi
    exit 0
fi
echo "Scan for additional .ini files in: {}"
"#,
                conf_d.display()
            ),
        );
        conf_d
    }

    #[tokio::test]
    async fn set_and_show_fill_launch_probed_cli_values_both_contexts() {
        let fx = fixture(false);
        let conf_d = install_fake_homebrew_php_with_values(&fx, "8.4");

        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        let normal = outcome
            .rows
            .iter()
            .find(|r| r.provider == "homebrew" && r.sapi == "cli" && r.context == "normal")
            .expect("homebrew cli normal row");
        assert_eq!(normal.observed.as_deref(), Some("1G"));
        assert_eq!(normal.observed_state, "launch-probed");
        let sanitized = outcome
            .rows
            .iter()
            .find(|r| r.provider == "homebrew" && r.sapi == "cli" && r.context == "sanitized")
            .expect("homebrew cli sanitized row");
        assert_eq!(
            sanitized.observed.as_deref(),
            Some("777M"),
            "sanitized context probes under the cleared env"
        );
        assert_eq!(sanitized.observed_state, "launch-probed");

        // The reconcile that materialized the channel file stamped the
        // Hearth-owned timestamp behind the pending-restart marker.
        let manifest = Manifest::load(&fx.engine.manifest_path()).unwrap();
        let entry = manifest
            .files
            .iter()
            .find(|e| e.path == conf_d.join(CHANNEL_FILE_NAME))
            .expect("manifest entry for the written channel file");
        assert!(
            entry.applied_at_unix_ms.is_some(),
            "applied_at_unix_ms stamped on materialization"
        );

        // Read-only Show reports the same launch-probed values.
        let shown = fx
            .engine
            .apply(PhpConfigAction::Show {
                key: Some("memory_limit".to_string()),
            })
            .await
            .unwrap();
        let normal = shown
            .rows
            .iter()
            .find(|r| r.provider == "homebrew" && r.sapi == "cli" && r.context == "normal")
            .unwrap();
        assert_eq!(normal.observed.as_deref(), Some("1G"));
        assert_eq!(normal.observed_state, "launch-probed");
    }

    #[tokio::test]
    async fn fpm_probe_runs_dash_i_under_service_env() {
        let fx = fixture(false);
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();
        // The fake FPM asserts the EXACT probe contract: `-i` (never
        // `--ini`/`-r`) and the exact Belt-E service env pair. Any other
        // invocation shape fails, so a passing test proves both.
        let expected_env = format!(":{}", fx.config_dir.join("php/8.4/conf.d").display());
        write_executable(
            &fx.config_dir.join("php/8.4/php-fpm"),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "--warmup" ]; then exit 0; fi
[ "$1" = "-i" ] || exit 7
[ "$PHP_INI_SCAN_DIR" = "{expected_env}" ] || exit 8
echo "memory_limit => 1G => 1G"
"#
            ),
        );

        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        let launched = outcome
            .rows
            .iter()
            .find(|r| r.sapi == "fpm" && r.context == "launched")
            .expect("supervised FPM row");
        assert_eq!(
            launched.observed.as_deref(),
            Some("1G"),
            "-i probe under the exact service env parsed the directive"
        );
        assert_eq!(launched.observed_state, "launch-probed");

        // The same binary's classification contexts are NOT effective-probed
        // (FPM has no `-r`; only the launched service row is) — their states
        // keep the truthful non-probe vocabulary.
        for row in outcome
            .rows
            .iter()
            .filter(|r| r.sapi == "fpm" && r.context != "launched")
        {
            assert_ne!(
                row.observed_state, "launch-probed",
                "only the launched service row may be launch-probed: {row:?}"
            );
        }
    }

    #[tokio::test]
    async fn probe_failure_renders_na_never_fails_command() {
        let fx = fixture(false);
        let roots_homebrew = fx.engine.provider_roots.homebrew.clone();
        let conf_d = roots_homebrew.join("etc/php/8.4/conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        // Classification works; every effective-value probe exits nonzero.
        write_executable(
            &roots_homebrew.join("opt/php@8.4/bin/php"),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "--warmup" ]; then exit 0; fi
if [ "$1" = "-r" ]; then exit 1; fi
echo "Scan for additional .ini files in: {}"
"#,
                conf_d.display()
            ),
        );

        // Read-only Show with nothing configured: probe failure → n/a.
        let shown = fx
            .engine
            .apply(PhpConfigAction::Show {
                key: Some("memory_limit".to_string()),
            })
            .await
            .unwrap();
        for row in shown.rows.iter().filter(|r| r.provider == "homebrew") {
            assert_eq!(row.observed, None, "failed probe never invents: {row:?}");
            assert_eq!(row.observed_state, "n/a");
        }

        // Mutating Set: probe failure leaves the truthful materialized state
        // and never fails the command.
        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert_eq!(outcome.persisted, Some(true));
        assert!(outcome.hard_failures().is_empty(), "{outcome:?}");
        let normal = outcome
            .rows
            .iter()
            .find(|r| r.provider == "homebrew" && r.sapi == "cli" && r.context == "normal")
            .unwrap();
        assert_eq!(
            normal.observed_state, "materialized",
            "probe failure preserves the channel-file state, never upgrades it"
        );
    }

    #[tokio::test]
    async fn ambient_external_fpm_is_structurally_never_executed_and_never_writable() {
        let fx = fixture(false);
        let herd = fx.engine.provider_roots.herd.clone();
        let user_channel = herd.join("config/php/84");
        std::fs::create_dir_all(&user_channel).unwrap();
        let log = herd.join("invocations.log");
        // FPM-only ambient provider; every non-warmup exec is logged FIRST.
        write_executable(
            &herd.join("bin/php84-fpm"),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "--warmup" ]; then exit 0; fi
echo run >> "{}"
echo "Scan for additional .ini files in: {}"
"#,
                log.display(),
                user_channel.display()
            ),
        );
        let count = |path: &Path| {
            std::fs::read_to_string(path)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };
        assert_eq!(count(&log), 0, "warmup must not log");

        // C1-2 (review 5650): the FIRST Status/Show call — classification
        // included — must execute the ambient FPM binary ZERO times. This is
        // the structural no-exec proof, not a cached-second-call proof.
        let first = fx
            .engine
            .apply(PhpConfigAction::Show {
                key: Some("memory_limit".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(
            count(&log),
            0,
            "ambient external FPM must be structurally unexecuted on the FIRST call"
        );
        let external = first
            .rows
            .iter()
            .find(|r| r.provider == "herd" && r.sapi == "fpm")
            .expect("external FPM row");
        assert_eq!(external.context, "external");
        assert_eq!(external.coverage, EXTERNAL_FPM_COVERAGE);
        assert_eq!(external.observed, None);
        assert_eq!(external.observed_state, "n/a");
        assert!(external.channel.is_none());

        // Mutating paths execute it exactly as often: never. And it can
        // never acquire a write channel.
        fx.engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert_eq!(count(&log), 0, "Set must not execute ambient FPM either");
        assert!(
            !user_channel.join(CHANNEL_FILE_NAME).exists(),
            "no write authority may derive from an ambient FPM target"
        );
    }

    // ---- C1-3 (review 5650): keyed status carries values ----

    #[tokio::test]
    async fn status_with_key_fills_configured_and_probed_values() {
        let fx = fixture(false);
        install_fake_homebrew_php_with_values(&fx, "8.4");
        fx.engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();

        let keyed = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: Some("memory_limit".to_string()),
                token: None,
            })
            .await
            .unwrap();
        let normal = keyed
            .rows
            .iter()
            .find(|r| r.provider == "homebrew" && r.sapi == "cli" && r.context == "normal")
            .expect("homebrew cli normal row");
        assert_eq!(normal.configured.as_deref(), Some("1G"));
        assert_eq!(normal.observed.as_deref(), Some("1G"));
        assert_eq!(normal.observed_state, "launch-probed");
        let sanitized = keyed
            .rows
            .iter()
            .find(|r| r.provider == "homebrew" && r.sapi == "cli" && r.context == "sanitized")
            .unwrap();
        assert_eq!(sanitized.observed.as_deref(), Some("777M"));
        assert_eq!(sanitized.observed_state, "launch-probed");

        // A keyless Status never fakes values (C1-3 contract).
        let keyless = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
        for row in &keyless.rows {
            assert_eq!(
                row.configured, None,
                "keyless status fakes no value: {row:?}"
            );
            assert_eq!(row.observed, None, "keyless status fakes no value: {row:?}");
            assert_eq!(row.observed_state, "n/a");
        }
    }

    // ---- C1-1 (review 5650): switch under Herd ----

    #[tokio::test]
    async fn switch_under_herd_performs_zero_fpm_supervisor_mutation() {
        use crate::service::supervisor::ManagedService;

        let fx = fixture(true); // Herd owns FPM
        write_executable(&fx.config_dir.join("php/8.3/php"), "#!/bin/sh\nexit 0\n");

        // A pre-existing registration (e.g. left from a Herd-absent boot)
        // must survive completely untouched: no stop, no replace, no
        // remove, no start. C6-1: register under explicit Unowned evidence,
        // then Herd takes ownership before the switch snapshot.
        let herd_owns = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_flag = Arc::clone(&herd_owns);
        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        {
            let mut sup = supervisor.lock().await;
            sup.set_fpm_ownership_probe(Arc::new(move || {
                if probe_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    FpmOwnership::Owned
                } else {
                    FpmOwnership::Unowned
                }
            }));
            sup.register(ManagedService::new(
                ServiceKind::PhpFpm,
                "/bin/echo".to_string(),
                vec!["sentinel".to_string()],
            ))
            .unwrap();
        }
        herd_owns.store(true, std::sync::atomic::Ordering::SeqCst);

        let msg = fx.engine.switch(&supervisor, "8.3").await.unwrap();
        assert!(msg.contains("php-fpm untouched"), "got: {msg}");
        assert!(msg.contains("Herd manages PHP-FPM"), "got: {msg}");

        {
            let sup = supervisor.lock().await;
            let svc = sup
                .service(ServiceKind::PhpFpm)
                .expect("registration must survive a Herd-guarded switch");
            assert_eq!(svc.command(), "/bin/echo", "no replacement");
            assert!(svc.started_at().is_none(), "never spawned");
        }

        // Version selection itself is preserved and persisted.
        assert_eq!(fx.config.lock().await.default_php, "8.3");
        let reloaded = HearthConfig::load_from(&fx.config_path).unwrap();
        assert_eq!(reloaded.default_php, "8.3");

        // And an empty supervisor gains NO registration either.
        let empty = Arc::new(Mutex::new(ServiceSupervisor::new()));
        empty
            .lock()
            .await
            .set_fpm_ownership_probe(Arc::new(|| FpmOwnership::Owned));
        fx.engine.switch(&empty, "8.3").await.unwrap();
        assert!(
            empty.lock().await.service(ServiceKind::PhpFpm).is_none(),
            "switch under Herd must never register FPM"
        );
    }

    // ---- C4-2 (review 5650 r4): unknown ownership fails closed ----

    /// Unknown ownership evidence must fail CLOSED across the config
    /// surfaces: restart policy reports the skip with the reason, switch
    /// leaves FPM untouched — zero supervisor mutation either way.
    #[tokio::test]
    async fn unknown_ownership_fails_closed_in_config_restart_and_switch() {
        use crate::service::supervisor::ManagedService;

        let fx = fixture_with_probe(Arc::new(|| {
            FpmOwnership::Unknown("ownership probe (pgrep) failed with status 2".to_string())
        }));
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();
        write_executable(&fx.config_dir.join("php/8.3/php"), "#!/bin/sh\nexit 0\n");

        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        {
            // C6-1: register + start under explicit Unowned evidence, THEN
            // swap in the Unknown probe — the switch below must consult the
            // supervisor's own snapshot and refuse fail-closed.
            let mut sup = supervisor.lock().await;
            sup.set_fpm_ownership_probe(Arc::new(|| FpmOwnership::Unowned));
            sup.register(ManagedService::new(
                ServiceKind::PhpFpm,
                "/bin/sleep".to_string(),
                vec!["30".to_string()],
            ))
            .unwrap();
            sup.start_service(ServiceKind::PhpFpm).unwrap();
            sup.set_fpm_ownership_probe(Arc::new(|| {
                FpmOwnership::Unknown("ownership probe (pgrep) failed with status 2".to_string())
            }));
        }
        let started = supervisor
            .lock()
            .await
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        // Config restart policy: actionable skip, zero mutation.
        let fpm = restart_fpm_conditionally(&supervisor, &fx.engine).await;
        match &fpm {
            FpmRestartOutcome::SkippedOwnershipUnknown { reason } => {
                assert!(reason.contains("pgrep"), "diagnostic surfaced: {reason}");
            }
            other => panic!("expected SkippedOwnershipUnknown, got {other:?}"),
        }
        {
            let sup = supervisor.lock().await;
            let svc = sup.service(ServiceKind::PhpFpm).unwrap();
            assert_eq!(svc.started_at(), Some(started), "zero stop/start");
        }

        // Switch: version persists, FPM untouched, reason surfaced.
        let msg = fx.engine.switch(&supervisor, "8.3").await.unwrap();
        assert!(msg.contains("php-fpm untouched"), "got: {msg}");
        assert!(msg.contains("ownership"), "got: {msg}");
        assert!(msg.contains("unknown"), "got: {msg}");
        {
            let sup = supervisor.lock().await;
            let svc = sup.service(ServiceKind::PhpFpm).unwrap();
            assert_eq!(
                svc.started_at(),
                Some(started),
                "switch under unknown: untouched"
            );
        }
        supervisor
            .lock()
            .await
            .stop_service(ServiceKind::PhpFpm)
            .unwrap();
    }

    // ---- C5-1C (review 5650 r6): Unknown never masquerades as Unowned ----

    /// Unknown ownership renders a LOUD ownership-unknown launched row with
    /// the diagnostic (never the "supervised" row) and performs zero FPM
    /// binary execution on Show AND Set. Fails at head c22d446 (Boolean
    /// adapter mapped Unknown → Herd-absent: supervised row + `-i` probe).
    #[tokio::test]
    async fn unknown_ownership_renders_loud_row_and_zero_fpm_exec() {
        let fx = fixture_with_probe(Arc::new(|| {
            FpmOwnership::Unknown("ownership probe (pgrep) failed with status 2".to_string())
        }));
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();
        let base = fx._tmp.path().canonicalize().unwrap();
        let log = base.join("fpm-invocations.log");
        write_executable(
            &fx.config_dir.join("php/8.4/php-fpm"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--warmup\" ]; then exit 0; fi\n\
                 echo run >> \"{}\"\n\
                 echo \"memory_limit => 1G => 1G\"\n",
                log.display()
            ),
        );
        let count = |path: &Path| {
            std::fs::read_to_string(path)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };

        let shown = fx
            .engine
            .apply(PhpConfigAction::Show {
                key: Some("memory_limit".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(count(&log), 0, "no -i execution under Unknown");
        let launched = shown
            .rows
            .iter()
            .find(|r| r.sapi == "fpm" && r.context == "launched")
            .expect("launched row stays present and loud");
        assert!(
            launched.coverage.contains("OWNERSHIP-UNKNOWN"),
            "got: {}",
            launched.coverage
        );
        assert!(
            launched.coverage.contains("pgrep"),
            "diagnostic surfaced: {}",
            launched.coverage
        );
        assert!(
            !launched.coverage.contains("supervised"),
            "must not claim supervised under Unknown: {}",
            launched.coverage
        );
        assert_eq!(launched.observed, None);

        // Mutation path: persistence proceeds, still zero FPM execution.
        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert_eq!(outcome.persisted, Some(true));
        assert_eq!(count(&log), 0, "Set must not execute FPM under Unknown");
    }

    /// C6-2 (review 5650 r7): the switch takes ONE authoritative ownership
    /// snapshot on the SUPERVISOR immediately before mutation. A flip to
    /// Owned landing before that snapshot yields a truthful skip with ZERO
    /// FPM mutation — and the snapshot is exactly one probe call (no split
    /// checks, no post-mutation recheck). Fails at head 79d54d2 (split
    /// engine-level checks; the supervisor's own probe was never the
    /// transaction authority).
    #[tokio::test]
    async fn switch_toctou_flip_to_owned_performs_zero_fpm_mutation() {
        use crate::service::supervisor::ManagedService;

        let fx = fixture(false);
        std::fs::create_dir_all(fx.config_dir.join("fpm")).unwrap();
        std::fs::write(fx.config_dir.join("fpm/php-fpm.conf"), "; fpm").unwrap();
        write_executable(&fx.config_dir.join("php/8.3/php"), "#!/bin/sh\nexit 0\n");
        write_spawnable_fpm(&fx.config_dir.join("php/8.3/php-fpm"));

        // Supervisor sequence probe: call #1 (the registration boundary
        // guard) proves Unowned; every later call — the switch's single
        // transaction snapshot — sees Owned. The counter pins the arity.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_calls = Arc::clone(&calls);
        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        {
            let mut sup = supervisor.lock().await;
            sup.set_fpm_ownership_probe(Arc::new(move || {
                let n = probe_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    FpmOwnership::Unowned
                } else {
                    FpmOwnership::Owned
                }
            }));
            sup.register(ManagedService::new(
                ServiceKind::PhpFpm,
                "/bin/echo".to_string(),
                vec!["sentinel".to_string()],
            ))
            .unwrap();
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "registration takes exactly one boundary snapshot"
        );

        let msg = fx.engine.switch(&supervisor, "8.3").await.unwrap();
        assert!(msg.contains("php-fpm untouched"), "got: {msg}");
        assert!(msg.contains("Herd manages PHP-FPM"), "got: {msg}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "switch takes exactly ONE ownership snapshot for the transaction"
        );
        let sup = supervisor.lock().await;
        let svc = sup
            .service(ServiceKind::PhpFpm)
            .expect("registration stands");
        assert_eq!(svc.command(), "/bin/echo", "zero register/reconfigure");
        assert!(svc.started_at().is_none(), "zero start");
        drop(sup);
        assert_eq!(
            fx.config.lock().await.default_php,
            "8.3",
            "version persisted"
        );
    }

    fn write_spawnable_fpm(path: &Path) {
        write_executable(path, "#!/bin/sh\nexec sleep 30\n");
    }

    // ---- C4-3 (review 5650 r4): ambiguous roots fail closed everywhere ----

    /// A post-construction root alias (herd → hearth) makes every hearth
    /// target's identity ambiguous: zero execution, loudly unverified rows,
    /// zero channel writes. Fails at head e6eedb3 (single-root membership).
    #[tokio::test]
    async fn aliased_roots_make_targets_unverified_zero_exec_zero_write() {
        let fx = fixture(false);
        let base = fx._tmp.path().canonicalize().unwrap();
        let log = base.join("invocations.log");
        let conf_d = fx.config_dir.join("php/8.4/conf.d");
        write_executable(
            &fx.config_dir.join("php/8.4/php"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--warmup\" ]; then exit 0; fi\n\
                 echo run >> \"{}\"\n\
                 echo \"Scan for additional .ini files in: {}\"\n",
                log.display(),
                conf_d.display()
            ),
        );
        // Alias AFTER construction (constructors reject overlapping sets):
        // base/herd → the hearth config dir.
        std::os::unix::fs::symlink(&fx.config_dir, &fx.engine.provider_roots.herd).unwrap();
        let count = |path: &Path| {
            std::fs::read_to_string(path)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };

        let outcome = fx
            .engine
            .apply(PhpConfigAction::Show {
                key: Some("memory_limit".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(count(&log), 0, "ambiguous identity is never executed");
        let row = outcome
            .rows
            .iter()
            .find(|r| r.provider == "hearth" && r.sapi == "cli" && r.context == "normal")
            .expect("hearth cli row renders loudly");
        assert!(
            row.coverage.contains("identity unverified") && row.coverage.contains("ambiguous"),
            "got: {}",
            row.coverage
        );

        fx.engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert_eq!(count(&log), 0, "Set must not execute either");
        assert!(
            !conf_d.join(CHANNEL_FILE_NAME).exists(),
            "no channel authority under an ambiguous identity"
        );
    }

    // ---- C3-3 (review 5650 r3): truthful discovery identity ----

    /// An expected layout path whose canonical target escapes its provider
    /// root is NEVER executed and renders loudly unverified with no write
    /// channel — instead of being probed under a false provider label.
    /// Fails at head 7c88222 (the escaped binary was executed/classified).
    #[tokio::test]
    async fn escaped_binary_target_is_unverified_and_never_executed() {
        let fx = fixture(false);
        let base = fx._tmp.path().canonicalize().unwrap();
        let herd = fx.engine.provider_roots.herd.clone();
        let log = base.join("invocations.log");
        // Outside script that logs every non-warmup exec.
        let outside = base.join("outside-php");
        write_executable(
            &outside,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--warmup\" ]; then exit 0; fi\n\
                 echo run >> \"{}\"\n\
                 echo \"Scan for additional .ini files in: /tmp\"\n",
                log.display()
            ),
        );
        std::fs::create_dir_all(herd.join("bin")).unwrap();
        std::os::unix::fs::symlink(&outside, herd.join("bin/php84")).unwrap();
        let count = |path: &Path| {
            std::fs::read_to_string(path)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };

        let outcome = fx
            .engine
            .apply(PhpConfigAction::Show {
                key: Some("memory_limit".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(count(&log), 0, "escaped binary must never be executed");
        let row = outcome
            .rows
            .iter()
            .find(|r| r.provider == "herd" && r.sapi == "cli" && r.context == "normal")
            .expect("unverified herd cli row still renders loudly");
        assert!(
            row.coverage.contains("identity unverified"),
            "got: {}",
            row.coverage
        );
        assert_eq!(row.observed, None);
        assert_eq!(row.observed_state, "n/a");

        // And a Set grants it nothing: zero execs, no herd channel write.
        fx.engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert_eq!(count(&log), 0, "Set must not execute the escaped binary");
        assert!(
            !herd.join("config/php/84").join(CHANNEL_FILE_NAME).exists(),
            "no write authority under a forged identity"
        );
    }

    // ---- C2-2/C2-4 (review 5650 r2): keyed-Status certificate + validation ----

    #[tokio::test]
    async fn status_certifies_applied_key_and_rejects_invalid_keys() {
        let fx = fixture(false);

        // Keyed Status echoes the exact key it applied (the capability
        // certificate a legacy daemon can never produce).
        let keyed = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: Some("memory_limit".to_string()),
                token: None,
            })
            .await
            .unwrap();
        assert_eq!(keyed.status_key.as_deref(), Some("memory_limit"));

        // Keyless Status carries no echo (legacy-identical wire form).
        let keyless = fx
            .engine
            .apply(PhpConfigAction::Status {
                key: None,
                token: None,
            })
            .await
            .unwrap();
        assert_eq!(keyless.status_key, None);

        // Invalid keys over the socket are stable actionable errors — never
        // a silent valueless report (C2-4 defense in depth), for Status AND
        // Show.
        for bad in ["bad key", "1leading", "inject\")); system(\"id"] {
            let err = fx
                .engine
                .apply(PhpConfigAction::Status {
                    key: Some(bad.to_string()),
                    token: None,
                })
                .await
                .unwrap_err();
            assert!(!err.is_empty(), "invalid status key must error");
            let err = fx
                .engine
                .apply(PhpConfigAction::Show {
                    key: Some(bad.to_string()),
                })
                .await
                .unwrap_err();
            assert!(!err.is_empty(), "invalid show key must error");
        }
    }

    // ---- C2-1 (review 5650 r2): mutation restart policy under live Herd ----

    /// Ownership flips false→true AFTER a Hearth FPM is registered and
    /// running: the centralized restart policy must leave the supervisor
    /// state logically untouched — no stop, start, restart, register, or
    /// remove — and report the truthful skip. Fails at head 08b7232 (the
    /// registered branch restarted unconditionally).
    #[tokio::test]
    async fn config_mutation_skips_registered_running_fpm_after_herd_becomes_live() {
        use crate::service::supervisor::ManagedService;
        use std::sync::atomic::Ordering;

        let (fx, herd_live) = fixture_with_herd_flag();
        let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
        {
            // C6-1: register + start under explicit Unowned evidence; the
            // restart policy under test consults the ENGINE's flip-aware
            // probe, so the supervisor keeps its registration-time evidence.
            let mut sup = supervisor.lock().await;
            sup.set_fpm_ownership_probe(Arc::new(|| FpmOwnership::Unowned));
            sup.register(ManagedService::new(
                ServiceKind::PhpFpm,
                "/bin/sleep".to_string(),
                vec!["30".to_string()],
            ))
            .unwrap();
            sup.start_service(ServiceKind::PhpFpm).unwrap();
        }
        let started_before = supervisor
            .lock()
            .await
            .service(ServiceKind::PhpFpm)
            .unwrap()
            .started_at()
            .expect("running");

        // Herd becomes live AFTER registration.
        herd_live.store(true, Ordering::SeqCst);

        // Persistence still succeeds…
        let outcome = fx
            .engine
            .apply(set_global("memory_limit", "1G"))
            .await
            .unwrap();
        assert_eq!(outcome.persisted, Some(true));

        // …and the centralized restart policy skips with ZERO mutation.
        let fpm = restart_fpm_conditionally(&supervisor, &fx.engine).await;
        assert_eq!(fpm, FpmRestartOutcome::SkippedHerdOwned);
        {
            let sup = supervisor.lock().await;
            let svc = sup.service(ServiceKind::PhpFpm).expect("still registered");
            assert!(
                matches!(svc.state, crate::service::ServiceState::Running { .. }),
                "still running: {:?}",
                svc.state
            );
            assert_eq!(
                svc.started_at(),
                Some(started_before),
                "same spawn — no stop/start happened"
            );
        }

        // Unregistered under Herd keeps the round-one truthful wording.
        {
            let mut sup = supervisor.lock().await;
            sup.stop_service(ServiceKind::PhpFpm).unwrap();
            sup.remove_service(ServiceKind::PhpFpm);
        }
        let fpm = restart_fpm_conditionally(&supervisor, &fx.engine).await;
        assert_eq!(fpm, FpmRestartOutcome::NotRegistered { herd_hint: true });
    }

    // ---- C7-1 (review 5650 r8): atomic config/MCP restart boundary ----

    /// The config/MCP restart's mutation is the supervisor's own atomic
    /// snapshot transaction. An Unowned→Owned or Unowned→Unknown flip
    /// landing AFTER the engine's read-only fast path is refused with ZERO
    /// FPM mutation. Fails at head 8b06320 (the old split
    /// stop-then-guarded-start stopped the child, then refused).
    #[tokio::test]
    async fn config_restart_flip_after_engine_check_performs_zero_fpm_mutation() {
        use crate::service::supervisor::ManagedService;

        for flip_to in ["owned", "unknown"] {
            let fx = fixture(false); // engine fast path reads Unowned
            let supervisor = Arc::new(Mutex::new(ServiceSupervisor::new()));
            {
                let mut sup = supervisor.lock().await;
                sup.set_fpm_ownership_probe(Arc::new(|| FpmOwnership::Unowned));
                sup.register(ManagedService::new(
                    ServiceKind::PhpFpm,
                    "/bin/sleep".to_string(),
                    vec!["30".to_string()],
                ))
                .unwrap();
                sup.start_service(ServiceKind::PhpFpm).unwrap();
                // The flip lands before the supervisor transaction snapshot.
                let flipped: crate::service::supervisor::ExternalFpmOwnership =
                    if flip_to == "owned" {
                        Arc::new(|| FpmOwnership::Owned)
                    } else {
                        Arc::new(|| {
                            FpmOwnership::Unknown(
                                "ownership probe (pgrep) failed with status 2".to_string(),
                            )
                        })
                    };
                sup.set_fpm_ownership_probe(flipped);
            }
            let started = supervisor
                .lock()
                .await
                .service(ServiceKind::PhpFpm)
                .unwrap()
                .started_at()
                .expect("running");

            let outcome = restart_fpm_conditionally(&supervisor, &fx.engine).await;
            match (flip_to, &outcome) {
                ("owned", FpmRestartOutcome::SkippedHerdOwned) => {}
                ("unknown", FpmRestartOutcome::SkippedOwnershipUnknown { reason }) => {
                    assert!(reason.contains("pgrep"), "{reason}");
                }
                other => panic!("truthful zero-mutation skip expected, got {other:?}"),
            }
            let mut sup = supervisor.lock().await;
            {
                let svc = sup.service(ServiceKind::PhpFpm).unwrap();
                assert!(
                    matches!(svc.state, crate::service::ServiceState::Running { .. }),
                    "zero mutation ({flip_to}): {:?}",
                    svc.state
                );
                assert_eq!(svc.started_at(), Some(started), "same spawn ({flip_to})");
            }
            sup.set_fpm_ownership_probe(Arc::new(|| FpmOwnership::Unowned));
            sup.stop_service(ServiceKind::PhpFpm).unwrap();
        }
    }
}
