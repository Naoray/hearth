//! Materialization of the canonical PHP INI store into verified channel
//! files, owned by an authoritative manifest with a crash-safe pending
//! journal, plus guarded migration of legacy per-version Hearth INIs.
//!
//! Ownership rule (S4): overwrite/delete/unmanage require a manifest entry
//! whose recorded sha256 exactly matches the on-disk file. The rendered file
//! header is informational only — a headered file without a manifest entry is
//! a collision and is refused, never adopted.

use crate::config::{HearthConfig, PhpIniSettings};
use crate::php::targets::{PhpProvider, PhpTarget, ProviderRoots, verify_user_channel};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// File name Hearth materializes into every verified channel. `zz-` sorts
/// last in the alphabetical scan order, so Hearth values win over Herd's
/// `php.ini` and Homebrew's `ext-*.ini`.
pub const CHANNEL_FILE_NAME: &str = "zz-hearth.ini";

pub const MANIFEST_VERSION: u64 = 1;

const REFUSED_REMEDIATION: &str = "remove or rename the file, then run `hearth php config --sync`";

/// Render the effective directive map into the byte-stable channel file.
/// The header is informational only — ownership lives in the manifest.
pub fn render_ini(directives: &BTreeMap<String, String>) -> String {
    let mut out = String::from(
        "; managed by hearth — do not edit. `hearth php config` regenerates this file.\n\
         ; hearth-owned: v1\n",
    );
    for (key, value) in directives {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn file_sha256(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| sha256_hex(&bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    Applied,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LastOutcome {
    Written,
    Unchanged,
    Deleted,
    Refused,
    Failed,
}

/// One manifest row. `sha256` is the hash of the last applied content;
/// `pending` rows additionally journal the transition hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: PathBuf,
    pub php_version: String,
    /// `herd-user` | `homebrew` | `hearth`
    pub channel: String,
    pub sha256: String,
    pub state: EntryState,
    /// Present on pending rows: hash the file must currently have
    /// (absent = the file must not exist yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_old_sha256: Option<String>,
    /// Present on pending rows: hash of the content being applied
    /// (absent = deletion intended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_sha256: Option<String>,
    pub last_outcome: LastOutcome,
}

/// Authoritative ownership record for every channel file Hearth has written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub files: Vec<ManifestEntry>,
}

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self {
                version: MANIFEST_VERSION,
                files: Vec::new(),
            });
        }
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    /// Atomic save: same-dir exclusive temp + rename (crash mid-save never
    /// truncates the manifest).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&parent)?;
        let content = toml::to_string_pretty(self)?;
        atomic_write(&parent, path, content.as_bytes(), 0o644)
    }

    fn entry_index(&self, path: &Path) -> Option<usize> {
        self.files.iter().position(|e| e.path == path)
    }
}

/// Write `bytes` to `dir/…/final_path` via an exclusive temp file + rename.
fn atomic_write(dir: &Path, final_path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let file_name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let mut attempt = 0u32;
    let (tmp_path, mut file) = loop {
        let candidate = dir.join(format!(
            ".{}.tmp.{}.{}",
            file_name,
            std::process::id(),
            attempt
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 1000 => {
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    };

    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp_path, final_path)?;
        crate::fsync::note_rename(final_path);
        // Durability barrier: the rename commits only once the containing
        // directory is synced. Failures propagate — never report success
        // past a failed barrier.
        crate::fsync::sync_dir(dir)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

/// Typed per-file outcome. One file's failure never aborts the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Written,
    Unchanged,
    Deleted,
    Refused { reason: String },
    Failed { error: String },
}

#[derive(Debug, Clone)]
pub struct FileAction {
    pub path: PathBuf,
    pub php_version: String,
    pub channel: String,
    pub outcome: WriteOutcome,
}

/// A target reconcile could not write for (privileged/best-effort channel):
/// reported, zero filesystem operations.
#[derive(Debug, Clone)]
pub struct SkippedTarget {
    pub id: crate::php::targets::PhpTargetIdentity,
    pub normal_channel: crate::php::targets::ChannelClass,
    pub sanitized_channel: crate::php::targets::ChannelClass,
}

#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    pub files: Vec<FileAction>,
    pub skipped: Vec<SkippedTarget>,
}

#[derive(Debug, Clone)]
pub enum RecoveryOutcome {
    /// The desired state was already on disk — manifest finalized.
    Finalized,
    /// The pre-write state is still on disk — entry stays journaled; the next
    /// reconcile completes the write.
    Retry,
    /// The file matches neither hash — collision; never touched.
    Refused { reason: String },
}

#[derive(Debug, Clone)]
pub struct RecoveryAction {
    pub path: PathBuf,
    pub outcome: RecoveryOutcome,
}

/// Resolve every pending journal entry deterministically. Idempotent; called
/// on daemon boot (before any reconcile) and before unmanage.
pub fn recover_pending(manifest_path: &Path) -> anyhow::Result<Vec<RecoveryAction>> {
    let mut manifest = Manifest::load(manifest_path)?;
    let mut actions = Vec::new();
    let mut changed = false;
    let mut remove: Vec<usize> = Vec::new();

    for (idx, entry) in manifest.files.iter_mut().enumerate() {
        if entry.state != EntryState::Pending {
            continue;
        }
        let actual = file_sha256(&entry.path);
        let outcome = match (&entry.desired_sha256, &actual) {
            // Write intended and completed → finalize.
            (Some(desired), Some(actual_sha)) if desired == actual_sha => {
                entry.sha256 = desired.clone();
                entry.state = EntryState::Applied;
                entry.expected_old_sha256 = None;
                entry.desired_sha256 = None;
                entry.last_outcome = LastOutcome::Written;
                RecoveryOutcome::Finalized
            }
            // Deletion intended and completed → drop the entry.
            (None, None) => {
                remove.push(idx);
                RecoveryOutcome::Finalized
            }
            // Pre-write state still on disk → leave journaled for retry.
            (_, actual_sha) if *actual_sha == entry.expected_old_sha256 => RecoveryOutcome::Retry,
            // Anything else is a collision — refuse, never touch.
            _ => {
                let reason = format!(
                    "{} changed outside Hearth during an interrupted update — {}",
                    entry.path.display(),
                    REFUSED_REMEDIATION
                );
                entry.state = EntryState::Applied;
                entry.expected_old_sha256 = None;
                entry.desired_sha256 = None;
                entry.last_outcome = LastOutcome::Refused;
                RecoveryOutcome::Refused { reason }
            }
        };
        if !matches!(outcome, RecoveryOutcome::Retry) {
            changed = true;
        }
        actions.push(RecoveryAction {
            path: entry.path.clone(),
            outcome,
        });
    }

    for idx in remove.into_iter().rev() {
        manifest.files.remove(idx);
    }
    if changed {
        manifest.save(manifest_path)?;
    }
    Ok(actions)
}

fn channel_kind(provider: PhpProvider) -> &'static str {
    match provider {
        PhpProvider::Hearth => "hearth",
        PhpProvider::Herd => "herd-user",
        PhpProvider::Homebrew => "homebrew",
    }
}

/// Is the file at `path` owned by this manifest entry — i.e. does the on-disk
/// hash exactly match the applied hash or a journaled transition hash?
fn entry_owns(entry: &ManifestEntry, actual: &str) -> bool {
    entry.sha256 == actual
        || entry.expected_old_sha256.as_deref() == Some(actual)
        || entry.desired_sha256.as_deref() == Some(actual)
}

/// Materialize the canonical store into every writable channel. Per-file
/// outcomes are isolated; privileged/best-effort targets get rows only.
pub fn reconcile(
    php_ini: &PhpIniSettings,
    targets: &[PhpTarget],
    manifest_path: &Path,
    roots: &ProviderRoots,
) -> ReconcileReport {
    let mut report = ReconcileReport::default();

    // Resolve any crashed journal entries first — deterministic recovery.
    if let Err(e) = recover_pending(manifest_path) {
        report.files.push(FileAction {
            path: manifest_path.to_path_buf(),
            php_version: String::new(),
            channel: String::new(),
            outcome: WriteOutcome::Failed {
                error: format!("journal recovery failed: {e}"),
            },
        });
        return report;
    }

    let mut manifest = match Manifest::load(manifest_path) {
        Ok(m) => m,
        Err(e) => {
            report.files.push(FileAction {
                path: manifest_path.to_path_buf(),
                php_version: String::new(),
                channel: String::new(),
                outcome: WriteOutcome::Failed {
                    error: format!("manifest unreadable: {e}"),
                },
            });
            return report;
        }
    };

    // Group writable targets by (version, channel dir) — CLI and FPM twins of
    // one provider share a single channel file.
    let mut groups: BTreeMap<(String, PathBuf), PhpProvider> = BTreeMap::new();
    for target in targets {
        match &target.write_channel {
            Some(dir) => {
                groups
                    .entry((target.id.version.clone(), dir.clone()))
                    .or_insert(target.id.provider);
            }
            None => report.skipped.push(SkippedTarget {
                id: target.id.clone(),
                normal_channel: target.normal_channel.clone(),
                sanitized_channel: target.sanitized_channel.clone(),
            }),
        }
    }

    let allowed = roots.allowed_prefixes();
    let mut produced: Vec<PathBuf> = Vec::new();

    for ((version, dir), provider) in groups {
        let file_path = dir.join(CHANNEL_FILE_NAME);
        produced.push(file_path.clone());
        let effective = php_ini.effective_for(&version);
        let kind = channel_kind(provider);

        let outcome = if effective.is_empty() {
            delete_channel_file(&mut manifest, manifest_path, &file_path, kind, &version)
        } else {
            let content = render_ini(&effective);
            write_channel_file(
                &mut manifest,
                manifest_path,
                &file_path,
                &dir,
                provider,
                kind,
                &version,
                &content,
                &allowed,
            )
        };

        if let Some(outcome) = outcome {
            report.files.push(FileAction {
                path: file_path,
                php_version: version,
                channel: kind.to_string(),
                outcome,
            });
        }
    }

    // Drop stale entries: not produced by any current group AND file gone.
    // A non-durable prune is reported as a typed failure; the stale entries
    // survive on disk and the next reconcile retries the prune.
    let before = manifest.files.len();
    manifest
        .files
        .retain(|e| produced.contains(&e.path) || e.path.exists());
    if manifest.files.len() != before
        && let Err(e) = manifest.save(manifest_path)
    {
        report.files.push(FileAction {
            path: manifest_path.to_path_buf(),
            php_version: String::new(),
            channel: String::new(),
            outcome: WriteOutcome::Failed {
                error: format!("stale manifest entries could not be pruned durably: {e}"),
            },
        });
    }

    report
}

/// One channel file's write path. Journal → verify → temp+rename → finalize.
/// Returns `None` for "nothing to do and nothing worth reporting".
#[allow(clippy::too_many_arguments)]
fn write_channel_file(
    manifest: &mut Manifest,
    manifest_path: &Path,
    file_path: &Path,
    dir: &Path,
    _provider: PhpProvider,
    kind: &str,
    version: &str,
    content: &str,
    allowed: &[PathBuf],
) -> Option<WriteOutcome> {
    let desired_sha = sha256_hex(content.as_bytes());
    let actual = file_sha256(file_path);

    if let Some(actual_sha) = &actual {
        match manifest.entry_index(file_path) {
            None => {
                return Some(WriteOutcome::Refused {
                    reason: format!(
                        "{} exists but is not tracked by the Hearth manifest — {}",
                        file_path.display(),
                        REFUSED_REMEDIATION
                    ),
                });
            }
            Some(idx) => {
                let entry = &manifest.files[idx];
                if !entry_owns(entry, actual_sha) {
                    return Some(WriteOutcome::Refused {
                        reason: format!(
                            "{} was modified outside Hearth — {}",
                            file_path.display(),
                            REFUSED_REMEDIATION
                        ),
                    });
                }
                if entry.sha256 == desired_sha
                    && entry.state == EntryState::Applied
                    && *actual_sha == desired_sha
                {
                    return Some(WriteOutcome::Unchanged);
                }
            }
        }
    }

    // 1. Journal the intent atomically BEFORE any channel mutation.
    let pending = ManifestEntry {
        path: file_path.to_path_buf(),
        php_version: version.to_string(),
        channel: kind.to_string(),
        sha256: actual.clone().unwrap_or_default(),
        state: EntryState::Pending,
        expected_old_sha256: actual,
        desired_sha256: Some(desired_sha.clone()),
        last_outcome: LastOutcome::Written,
    };
    match manifest.entry_index(file_path) {
        Some(idx) => manifest.files[idx] = pending,
        None => manifest.files.push(pending),
    }
    if let Err(e) = manifest.save(manifest_path) {
        return Some(WriteOutcome::Failed {
            error: format!("could not journal pending write: {e}"),
        });
    }

    // 2. Re-verify the channel and write via exclusive temp + rename.
    //    NO creation here (review 5594 B2-5): the engine performs hardened,
    //    verified Hearth-channel creation BEFORE reconcile. A directory that
    //    disappeared or was substituted after classification is a typed
    //    failure — never recreated through a possibly-changed path.
    if !dir.exists() {
        return finalize_failure(
            manifest,
            manifest_path,
            file_path,
            format!(
                "channel directory {} disappeared after classification — refusing to \
                 recreate at write time; run `hearth php config --sync`",
                dir.display()
            ),
        );
    }
    let verified = match verify_user_channel(dir, allowed) {
        Ok(v) => v,
        Err(rejection) => {
            let idx = manifest.entry_index(file_path).unwrap();
            manifest.files[idx].state = EntryState::Applied;
            manifest.files[idx].expected_old_sha256 = None;
            manifest.files[idx].desired_sha256 = None;
            manifest.files[idx].last_outcome = LastOutcome::Refused;
            let mut reason = format!("channel blocked: {rejection}");
            // The refusal stays primary, but a failure to durably record it
            // must not be discarded.
            if let Err(e) = manifest.save(manifest_path) {
                reason = format!(
                    "{reason}; additionally, recording this refusal in the manifest failed: {e}"
                );
            }
            return Some(WriteOutcome::Refused { reason });
        }
    };

    if let Err(e) = atomic_write(dir, file_path, content.as_bytes(), 0o644) {
        return finalize_failure(manifest, manifest_path, file_path, e.to_string());
    }

    // Dev/inode identity fallback (no dirfd binding): the directory we
    // verified must be the directory we wrote into.
    match std::fs::metadata(dir) {
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            if meta.dev() != verified.dev || meta.ino() != verified.ino {
                return finalize_failure(
                    manifest,
                    manifest_path,
                    file_path,
                    "channel directory was replaced during the write".to_string(),
                );
            }
        }
        Err(e) => return finalize_failure(manifest, manifest_path, file_path, e.to_string()),
    }

    // 3. Finalize the manifest entry.
    let idx = manifest.entry_index(file_path).unwrap();
    manifest.files[idx].sha256 = desired_sha;
    manifest.files[idx].state = EntryState::Applied;
    manifest.files[idx].expected_old_sha256 = None;
    manifest.files[idx].desired_sha256 = None;
    manifest.files[idx].last_outcome = LastOutcome::Written;
    if let Err(e) = manifest.save(manifest_path) {
        return Some(WriteOutcome::Failed {
            error: format!("written but manifest finalization failed: {e}"),
        });
    }
    Some(WriteOutcome::Written)
}

/// Record an io failure on the journaled entry, keeping it pending so the
/// next recovery/reconcile can retry deterministically.
fn finalize_failure(
    manifest: &mut Manifest,
    manifest_path: &Path,
    file_path: &Path,
    error: String,
) -> Option<WriteOutcome> {
    let mut error = error;
    if let Some(idx) = manifest.entry_index(file_path) {
        manifest.files[idx].last_outcome = LastOutcome::Failed;
        // The original failure stays primary, but a failure to durably
        // record it must not be discarded.
        if let Err(e) = manifest.save(manifest_path) {
            error = format!(
                "{error}; additionally, recording this failure in the manifest failed: {e}"
            );
        }
    }
    Some(WriteOutcome::Failed { error })
}

/// Empty effective map → delete the channel file, but only when the manifest
/// owns the exact on-disk bytes.
fn delete_channel_file(
    manifest: &mut Manifest,
    manifest_path: &Path,
    file_path: &Path,
    kind: &str,
    version: &str,
) -> Option<WriteOutcome> {
    let actual = match file_sha256(file_path) {
        Some(sha) => sha,
        None => {
            // Nothing on disk; drop any leftover entry — but ownership is
            // released only if that cleanup is durable.
            if let Some(idx) = manifest.entry_index(file_path) {
                let entry = manifest.files.remove(idx);
                if let Err(e) = manifest.save(manifest_path) {
                    manifest.files.insert(idx, entry);
                    return Some(WriteOutcome::Failed {
                        error: format!(
                            "could not durably release ownership of missing {}: {e}",
                            file_path.display()
                        ),
                    });
                }
            }
            return None;
        }
    };

    let owned = manifest
        .entry_index(file_path)
        .map(|idx| entry_owns(&manifest.files[idx], &actual))
        .unwrap_or(false);
    if !owned {
        return Some(WriteOutcome::Refused {
            reason: format!(
                "{} is not tracked by the Hearth manifest — {}",
                file_path.display(),
                REFUSED_REMEDIATION
            ),
        });
    }

    // Journal the deletion (desired absent), delete, then drop the entry.
    let idx = manifest.entry_index(file_path).unwrap();
    manifest.files[idx].php_version = version.to_string();
    manifest.files[idx].channel = kind.to_string();
    manifest.files[idx].state = EntryState::Pending;
    manifest.files[idx].expected_old_sha256 = Some(actual);
    manifest.files[idx].desired_sha256 = None;
    if let Err(e) = manifest.save(manifest_path) {
        return Some(WriteOutcome::Failed {
            error: format!("could not journal pending delete: {e}"),
        });
    }
    // Removal barrier: the unlink is durable only after the containing
    // directory syncs; ownership is dropped only past that barrier.
    let removal = std::fs::remove_file(file_path).and_then(|()| {
        crate::fsync::note_remove(file_path);
        let parent = file_path.parent().unwrap_or(Path::new("."));
        crate::fsync::sync_dir(parent)
    });
    if let Err(e) = removal {
        return finalize_failure(manifest, manifest_path, file_path, e.to_string());
    }
    // `Deleted` is reported only after the ownership drop is durable. On
    // failure the entry is restored in memory to mirror the durable pending
    // journal on disk; recovery converges from either state.
    let idx = manifest.entry_index(file_path).unwrap();
    let entry = manifest.files.remove(idx);
    if let Err(e) = manifest.save(manifest_path) {
        manifest.files.insert(idx, entry);
        return Some(WriteOutcome::Failed {
            error: format!("channel file removed but manifest finalization failed: {e}"),
        });
    }
    Some(WriteOutcome::Deleted)
}

/// Remove every Hearth-written channel file (journal-resolved, exact-hash
/// deletions only). Hash-mismatched survivors are reported Refused.
pub fn unmanage(manifest_path: &Path) -> anyhow::Result<Vec<FileAction>> {
    recover_pending(manifest_path)?;
    let mut manifest = Manifest::load(manifest_path)?;
    let mut actions = Vec::new();
    let mut keep = Vec::new();

    for entry in manifest.files.drain(..) {
        match file_sha256(&entry.path) {
            None => {} // already gone — drop the entry silently
            Some(actual) if actual == entry.sha256 => {
                // Ownership is dropped only after the unlink is durable in
                // its containing directory.
                let removal = std::fs::remove_file(&entry.path).and_then(|()| {
                    crate::fsync::note_remove(&entry.path);
                    let parent = entry.path.parent().unwrap_or(Path::new("."));
                    crate::fsync::sync_dir(parent)
                });
                match removal {
                    Ok(()) => actions.push(FileAction {
                        path: entry.path.clone(),
                        php_version: entry.php_version.clone(),
                        channel: entry.channel.clone(),
                        outcome: WriteOutcome::Deleted,
                    }),
                    Err(e) => {
                        actions.push(FileAction {
                            path: entry.path.clone(),
                            php_version: entry.php_version.clone(),
                            channel: entry.channel.clone(),
                            outcome: WriteOutcome::Failed {
                                error: e.to_string(),
                            },
                        });
                        keep.push(entry);
                    }
                }
            }
            Some(_) => {
                actions.push(FileAction {
                    path: entry.path.clone(),
                    php_version: entry.php_version.clone(),
                    channel: entry.channel.clone(),
                    outcome: WriteOutcome::Refused {
                        reason: format!(
                            "{} was modified outside Hearth — left in place",
                            entry.path.display()
                        ),
                    },
                });
                keep.push(entry);
            }
        }
    }
    manifest.files = keep;
    manifest.save(manifest_path)?;
    Ok(actions)
}

// ---- legacy migration ----

#[derive(Debug, Clone)]
pub struct SkippedDirective {
    pub version: String,
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct MigrationReport {
    pub imported: Vec<(String, String)>,
    pub skipped: Vec<SkippedDirective>,
    pub migrated_files: Vec<PathBuf>,
}

/// Import legacy `config_dir/php/{v}/php.ini` files into
/// `php_ini.overrides[v]`, every key/value gated through `ini_guard`.
/// The rename to `php.ini.migrated.bak[.N]` is the idempotency marker; a
/// second run finds no `php.ini` and is a no-op.
pub fn migrate_legacy_inis(
    config: &mut HearthConfig,
    config_path: &Path,
    php_dir: &Path,
) -> anyhow::Result<MigrationReport> {
    use crate::php::ini_guard;

    let mut report = MigrationReport::default();
    let Ok(entries) = std::fs::read_dir(php_dir) else {
        return Ok(report); // nothing to migrate
    };

    // Stage phase: parse, guard, and import everything in memory. Nothing on
    // disk is touched until the canonical config has been persisted — the
    // legacy php.ini is the only automatic import source, so it must survive
    // any persistence failure for a restart to retry.
    let snapshot = config.php_ini.clone();
    let mut to_rename: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let version = entry.file_name().to_string_lossy().into_owned();
        let legacy = entry.path().join("php.ini");
        if !legacy.is_file() {
            continue;
        }

        let mut ini = configparser::ini::Ini::new();
        let map = match ini.load(&legacy) {
            Ok(map) => map,
            Err(e) => {
                report.skipped.push(SkippedDirective {
                    version: version.clone(),
                    key: String::new(),
                    reason: format!("unparseable legacy INI {}: {e}", legacy.display()),
                });
                continue;
            }
        };

        for (_section, directives) in map {
            for (key, value) in directives {
                let value = value.unwrap_or_default();
                if let Err(e) = ini_guard::validate_key(&key) {
                    report.skipped.push(SkippedDirective {
                        version: version.clone(),
                        key,
                        reason: e.to_string(),
                    });
                    continue;
                }
                if let Err(e) = ini_guard::validate_value(&value) {
                    report.skipped.push(SkippedDirective {
                        version: version.clone(),
                        key,
                        reason: e.to_string(),
                    });
                    continue;
                }
                let overrides = config.php_ini.overrides.entry(version.clone()).or_default();
                if !overrides.contains_key(&key) {
                    overrides.insert(key.clone(), value);
                    report.imported.push((version.clone(), key));
                }
            }
        }

        to_rename.push(legacy);
    }

    if to_rename.is_empty() {
        return Ok(report);
    }

    // Persist the canonical config BEFORE renaming any legacy source. On
    // failure, revert the staged imports and leave every php.ini untouched.
    if config.php_ini != snapshot
        && let Err(e) = config.save_to(config_path)
    {
        config.php_ini = snapshot;
        return Err(e);
    }

    // Canonical values are durable — now retire the sources with unique
    // no-clobber backups (.bak, .bak.1, …). A rename failure here is safe:
    // the surviving php.ini re-imports as a no-op and the rename retries.
    for legacy in to_rename {
        let mut backup = legacy.with_file_name("php.ini.migrated.bak");
        let mut counter = 0u32;
        while backup.exists() {
            counter += 1;
            backup = legacy.with_file_name(format!("php.ini.migrated.bak.{counter}"));
        }
        std::fs::rename(&legacy, &backup)?;
        crate::fsync::note_rename(&backup);
        // Retirement is complete only once the source directory entry change
        // is durable. Canonical values are already durable, so a failure here
        // returns an error and the surviving php.ini retries idempotently.
        let source_dir = legacy.parent().unwrap_or(Path::new("."));
        crate::fsync::sync_dir(source_dir)?;
        report.migrated_files.push(legacy);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PhpIniSettings;
    use crate::php::targets::{
        ChannelClass, PhpProvider, PhpSapi, PhpTarget, PhpTargetIdentity, ProbeInfo, ProviderRoots,
        expected_channel_dir,
    };
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn canon_tmp() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = tmp.path().canonicalize().unwrap();
        (tmp, canonical)
    }

    fn test_roots(base: &Path) -> ProviderRoots {
        ProviderRoots::isolated(
            base,
            base.join("hearth"),
            base.join("Herd"),
            base.join("homebrew"),
        )
        .unwrap()
    }

    fn verified_target(
        provider: PhpProvider,
        version: &str,
        sapi: PhpSapi,
        dir: &Path,
    ) -> PhpTarget {
        // B2-5 contract: a Verified channel dir EXISTS before reconcile (the
        // engine's hardened creation runs earlier); reconcile itself never
        // creates directories.
        std::fs::create_dir_all(dir).unwrap();
        PhpTarget {
            id: PhpTargetIdentity {
                provider,
                version: version.to_string(),
                sapi,
                binary: PathBuf::from("/fake/php"),
            },
            probe: ProbeInfo::default(),
            normal_channel: ChannelClass::Verified {
                dir: dir.to_path_buf(),
            },
            sanitized_channel: ChannelClass::BestEffort {
                reason: "test".to_string(),
            },
            write_channel: Some(dir.to_path_buf()),
        }
    }

    fn unwritable_target(provider: PhpProvider, version: &str, class: ChannelClass) -> PhpTarget {
        PhpTarget {
            id: PhpTargetIdentity {
                provider,
                version: version.to_string(),
                sapi: PhpSapi::Cli,
                binary: PathBuf::from("/fake/php"),
            },
            probe: ProbeInfo::default(),
            normal_channel: class.clone(),
            sanitized_channel: class,
            write_channel: None,
        }
    }

    fn simple_ini(version: &str, key: &str, value: &str) -> PhpIniSettings {
        let mut ini = PhpIniSettings::default();
        ini.overrides
            .entry(version.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
        ini
    }

    fn outcome_for<'r>(report: &'r ReconcileReport, path: &Path) -> &'r WriteOutcome {
        &report
            .files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("no file action for {}: {report:?}", path.display()))
            .outcome
    }

    // ---- rendering ----

    #[test]
    fn render_stable_and_headered() {
        let mut directives = BTreeMap::new();
        directives.insert("memory_limit".to_string(), "2G".to_string());
        let first = render_ini(&directives);
        let second = render_ini(&directives);
        assert_eq!(first, second, "rendering must be byte-stable");
        assert_eq!(
            first,
            "; managed by hearth — do not edit. `hearth php config` regenerates this file.\n\
             ; hearth-owned: v1\n\
             memory_limit=2G\n"
        );
    }

    // ---- reconcile write paths ----

    #[test]
    fn reconcile_writes_new_file_atomically() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        // Hearth-owned channel: reconcile may create the conf.d itself.
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let ini = simple_ini("8.4", "memory_limit", "2G");
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        let report = reconcile(&ini, &[target], &manifest_path, &roots);

        let file = dir.join(CHANNEL_FILE_NAME);
        assert!(matches!(outcome_for(&report, &file), WriteOutcome::Written));
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, render_ini(&ini.effective_for("8.4")));

        // Only the channel file lives in the dir — no temp litter.
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(CHANNEL_FILE_NAME)]);

        let manifest = Manifest::load(&manifest_path).unwrap();
        let entry = manifest.files.iter().find(|e| e.path == file).unwrap();
        assert_eq!(entry.state, EntryState::Applied);
        assert_eq!(entry.sha256, sha256_hex(content.as_bytes()));
        assert_eq!(entry.last_outcome, LastOutcome::Written);
    }

    #[test]
    fn reconcile_unchanged_is_noop() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let ini = simple_ini("8.4", "memory_limit", "2G");
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        reconcile(&ini, std::slice::from_ref(&target), &manifest_path, &roots);
        let file = dir.join(CHANNEL_FILE_NAME);
        let before = std::fs::read(&file).unwrap();

        let report = reconcile(&ini, &[target], &manifest_path, &roots);
        assert!(matches!(
            outcome_for(&report, &file),
            WriteOutcome::Unchanged
        ));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        let manifest = Manifest::load(&manifest_path).unwrap();
        assert_eq!(manifest.files.len(), 1);
    }

    #[test]
    fn reconcile_refuses_untracked_file_even_with_header() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = roots.herd.join("config/php/84");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CHANNEL_FILE_NAME);
        // Correct header, no manifest entry: still a collision (S4).
        let impostor = "; managed by hearth — do not edit. `hearth php config` regenerates this file.\n\
                        ; hearth-owned: v1\n\
                        memory_limit=9G\n";
        std::fs::write(&file, impostor).unwrap();

        let manifest_path = base.join("hearth/php/manifest.toml");
        let ini = simple_ini("8.4", "memory_limit", "2G");
        let target = verified_target(PhpProvider::Herd, "8.4", PhpSapi::Cli, &dir);

        let report = reconcile(&ini, &[target], &manifest_path, &roots);
        match outcome_for(&report, &file) {
            WriteOutcome::Refused { reason } => {
                assert!(
                    reason.contains("--sync"),
                    "refusal must carry remediation: {reason}"
                );
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            impostor,
            "untracked file must remain untouched"
        );
    }

    #[test]
    fn reconcile_updates_own_file_exact_hash() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );

        let updated = simple_ini("8.4", "memory_limit", "4G");
        let report = reconcile(&updated, &[target], &manifest_path, &roots);
        let file = dir.join(CHANNEL_FILE_NAME);
        assert!(matches!(outcome_for(&report, &file), WriteOutcome::Written));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            render_ini(&updated.effective_for("8.4"))
        );
    }

    #[test]
    fn pending_journal_written_before_mutation() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = roots.herd.join("config/php/84");
        std::fs::create_dir_all(&dir).unwrap();
        // Owner-unwritable dir passes channel verification (only the group/other
        // write mask is checked) but temp-file creation fails — after the
        // journal entry was persisted.
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&dir, perms).unwrap();

        let manifest_path = base.join("hearth/php/manifest.toml");
        let ini = simple_ini("8.4", "memory_limit", "2G");
        let target = verified_target(PhpProvider::Herd, "8.4", PhpSapi::Cli, &dir);
        let report = reconcile(&ini, &[target], &manifest_path, &roots);

        let file = dir.join(CHANNEL_FILE_NAME);
        assert!(matches!(
            outcome_for(&report, &file),
            WriteOutcome::Failed { .. }
        ));

        let manifest = Manifest::load(&manifest_path).unwrap();
        let entry = manifest.files.iter().find(|e| e.path == file).unwrap();
        assert_eq!(
            entry.state,
            EntryState::Pending,
            "journal entry must exist even though the write never happened"
        );
        assert_eq!(entry.last_outcome, LastOutcome::Failed);

        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dir, perms).unwrap();
    }

    // ---- crash recovery ----

    fn pending_manifest(
        manifest_path: &Path,
        file: &Path,
        expected_old: Option<&str>,
        desired: Option<&str>,
    ) {
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            files: vec![ManifestEntry {
                path: file.to_path_buf(),
                php_version: "8.4".to_string(),
                channel: "herd-user".to_string(),
                sha256: expected_old.unwrap_or_default().to_string(),
                state: EntryState::Pending,
                expected_old_sha256: expected_old.map(str::to_string),
                desired_sha256: desired.map(str::to_string),
                last_outcome: LastOutcome::Written,
            }],
        };
        manifest.save(manifest_path).unwrap();
    }

    #[test]
    fn crash_recovery_finalizes_matching_desired_hash() {
        let (_tmp, base) = canon_tmp();
        let dir = base.join("channel");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CHANNEL_FILE_NAME);
        let desired_content = "; managed by hearth\nmemory_limit=2G\n";
        // Crash happened after rename, before manifest finalization.
        std::fs::write(&file, desired_content).unwrap();
        let desired_sha = sha256_hex(desired_content.as_bytes());

        let manifest_path = base.join("manifest.toml");
        pending_manifest(&manifest_path, &file, None, Some(&desired_sha));

        let actions = recover_pending(&manifest_path).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0].outcome, RecoveryOutcome::Finalized));

        let manifest = Manifest::load(&manifest_path).unwrap();
        let entry = &manifest.files[0];
        assert_eq!(entry.state, EntryState::Applied);
        assert_eq!(entry.sha256, desired_sha);
    }

    #[test]
    fn crash_recovery_retries_matching_expected_old() {
        let (_tmp, base) = canon_tmp();
        let dir = base.join("channel");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CHANNEL_FILE_NAME);
        let old_content = "; managed by hearth\nmemory_limit=1G\n";
        // Crash happened before the rename: the old file is still in place.
        std::fs::write(&file, old_content).unwrap();
        let old_sha = sha256_hex(old_content.as_bytes());
        let desired_sha = sha256_hex(b"something newer");

        let manifest_path = base.join("manifest.toml");
        pending_manifest(&manifest_path, &file, Some(&old_sha), Some(&desired_sha));

        let actions = recover_pending(&manifest_path).unwrap();
        assert!(matches!(actions[0].outcome, RecoveryOutcome::Retry));
        let manifest = Manifest::load(&manifest_path).unwrap();
        assert_eq!(
            manifest.files[0].state,
            EntryState::Pending,
            "retryable entry stays journaled for the next reconcile"
        );
    }

    #[test]
    fn crash_recovery_refuses_unknown_hash() {
        let (_tmp, base) = canon_tmp();
        let dir = base.join("channel");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CHANNEL_FILE_NAME);
        let foreign = "someone else's content\n";
        std::fs::write(&file, foreign).unwrap();

        let manifest_path = base.join("manifest.toml");
        pending_manifest(
            &manifest_path,
            &file,
            Some(&sha256_hex(b"old")),
            Some(&sha256_hex(b"new")),
        );

        let actions = recover_pending(&manifest_path).unwrap();
        assert!(matches!(
            actions[0].outcome,
            RecoveryOutcome::Refused { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            foreign,
            "unknown content is never touched"
        );
        let manifest = Manifest::load(&manifest_path).unwrap();
        assert_eq!(manifest.files[0].last_outcome, LastOutcome::Refused);
        assert_eq!(manifest.files[0].state, EntryState::Applied);
    }

    // ---- deletion / isolation / staleness ----

    #[test]
    fn reconcile_deletes_only_manifest_tracked() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let hearth_dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let herd_dir = roots.herd.join("config/php/84");
        std::fs::create_dir_all(&herd_dir).unwrap();
        let manifest_path = base.join("hearth/php/manifest.toml");
        let hearth_target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &hearth_dir);
        let herd_target = verified_target(PhpProvider::Herd, "8.4", PhpSapi::Cli, &herd_dir);

        // Track only the hearth file through a real reconcile.
        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            std::slice::from_ref(&hearth_target),
            &manifest_path,
            &roots,
        );
        let tracked = hearth_dir.join(CHANNEL_FILE_NAME);
        assert!(tracked.exists());

        // Untracked file in the herd channel.
        let untracked = herd_dir.join(CHANNEL_FILE_NAME);
        std::fs::write(&untracked, "; hearth-owned: v1\nmemory_limit=9G\n").unwrap();

        // Empty effective map → deletion pass over both channels.
        let report = reconcile(
            &PhpIniSettings::default(),
            &[hearth_target, herd_target],
            &manifest_path,
            &roots,
        );

        assert!(matches!(
            outcome_for(&report, &tracked),
            WriteOutcome::Deleted
        ));
        assert!(!tracked.exists());
        assert!(matches!(
            outcome_for(&report, &untracked),
            WriteOutcome::Refused { .. }
        ));
        assert!(untracked.exists(), "untracked file survives");
    }

    #[test]
    fn reconcile_never_touches_privileged_or_besteffort() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let targets = [
            unwritable_target(
                PhpProvider::Herd,
                "8.4",
                ChannelClass::PrivilegedDir {
                    dir: PathBuf::from("/usr/local/etc/php/conf.d"),
                },
            ),
            unwritable_target(
                PhpProvider::Homebrew,
                "8.1",
                ChannelClass::BestEffort {
                    reason: "probe failed".to_string(),
                },
            ),
        ];

        let report = reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &targets,
            &manifest_path,
            &roots,
        );

        assert!(
            report.files.is_empty(),
            "zero filesystem outcomes: {report:?}"
        );
        assert_eq!(report.skipped.len(), 2, "both targets get report rows");
        assert!(
            Manifest::load(&manifest_path).unwrap().files.is_empty(),
            "no manifest entries for unwritable targets"
        );
    }

    #[test]
    fn partial_failure_isolated() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let good_dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let bad_dir = roots.herd.join("config/php/84");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let mut perms = std::fs::metadata(&bad_dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&bad_dir, perms).unwrap();

        let manifest_path = base.join("hearth/php/manifest.toml");
        let ini = simple_ini("8.4", "memory_limit", "2G");
        let report = reconcile(
            &ini,
            &[
                verified_target(PhpProvider::Herd, "8.4", PhpSapi::Cli, &bad_dir),
                verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &good_dir),
            ],
            &manifest_path,
            &roots,
        );

        assert!(matches!(
            outcome_for(&report, &bad_dir.join(CHANNEL_FILE_NAME)),
            WriteOutcome::Failed { .. }
        ));
        assert!(
            matches!(
                outcome_for(&report, &good_dir.join(CHANNEL_FILE_NAME)),
                WriteOutcome::Written
            ),
            "one channel's failure must not abort the other: {report:?}"
        );

        let mut perms = std::fs::metadata(&bad_dir).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bad_dir, perms).unwrap();
    }

    #[test]
    fn stale_manifest_entry_dropped() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);
        let ini = simple_ini("8.4", "memory_limit", "2G");

        reconcile(&ini, &[target], &manifest_path, &roots);
        let file = dir.join(CHANNEL_FILE_NAME);
        assert!(file.exists());

        // Channel disappears (user removed it) — file gone, entry stale.
        std::fs::remove_file(&file).unwrap();
        let report = reconcile(&ini, &[], &manifest_path, &roots);
        assert!(report.files.is_empty(), "{report:?}");
        assert!(
            Manifest::load(&manifest_path).unwrap().files.is_empty(),
            "stale entry (file no longer exists) must be dropped"
        );
    }

    // ---- durability barriers (round-2) ----

    use crate::fsync::test_hooks as barriers;

    #[test]
    fn pending_manifest_sync_failure_prevents_channel_mutation() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();

        barriers::reset();
        let _failpoint = barriers::fail_sync_of(&manifest_dir);
        let report = reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &dir,
            )],
            &manifest_path,
            &roots,
        );
        let events = barriers::events();
        barriers::reset();

        let file = dir.join(CHANNEL_FILE_NAME);
        assert!(
            matches!(outcome_for(&report, &file), WriteOutcome::Failed { .. }),
            "{report:?}"
        );
        assert!(
            !file.exists(),
            "channel must not be mutated when the pending journal is not durable"
        );
        assert!(
            !events
                .iter()
                .any(|e| e == &format!("rename:{}", file.display())),
            "no channel rename may precede a durable pending journal: {events:?}"
        );
    }

    #[test]
    fn channel_sync_failure_leaves_recoverable_pending_state() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = base.join("hearth/php/manifest.toml");
        let ini = simple_ini("8.4", "memory_limit", "2G");

        barriers::reset();
        let failpoint = barriers::fail_sync_of(&dir);
        let report = reconcile(
            &ini,
            &[verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &dir,
            )],
            &manifest_path,
            &roots,
        );
        drop(failpoint);

        let file = dir.join(CHANNEL_FILE_NAME);
        assert!(
            matches!(outcome_for(&report, &file), WriteOutcome::Failed { .. }),
            "channel dir sync failure must be a typed failure: {report:?}"
        );
        let manifest = Manifest::load(&manifest_path).unwrap();
        let entry = manifest.files.iter().find(|e| e.path == file).unwrap();
        assert_eq!(
            entry.state,
            EntryState::Pending,
            "ownership must NOT be finalized past a failed channel barrier"
        );

        // The durable pending journal must remain recoverable: with the
        // failpoint cleared, recovery finalizes from the on-disk state.
        let actions = recover_pending(&manifest_path).unwrap();
        assert!(matches!(actions[0].outcome, RecoveryOutcome::Finalized));
        let manifest = Manifest::load(&manifest_path).unwrap();
        assert_eq!(manifest.files[0].state, EntryState::Applied);
    }

    #[test]
    fn reconcile_barrier_ordering_is_pending_channel_final() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();

        barriers::reset();
        let report = reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &dir,
            )],
            &manifest_path,
            &roots,
        );
        let events = barriers::events();
        barriers::reset();

        let file = dir.join(CHANNEL_FILE_NAME);
        assert!(matches!(outcome_for(&report, &file), WriteOutcome::Written));

        let manifest_sync = format!("sync:{}", manifest_dir.display());
        let manifest_sync_idxs: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| **e == manifest_sync)
            .map(|(i, _)| i)
            .collect();
        assert!(
            manifest_sync_idxs.len() >= 2,
            "pending AND final manifest saves must both sync the manifest dir: {events:?}"
        );
        let rename_channel = barriers::index_of(&events, &format!("rename:{}", file.display()));
        let sync_channel = barriers::index_of(&events, &format!("sync:{}", dir.display()));
        assert!(
            manifest_sync_idxs[0] < rename_channel
                && rename_channel < sync_channel
                && sync_channel < *manifest_sync_idxs.last().unwrap(),
            "required order: pending manifest sync → channel rename → channel dir sync → final manifest sync; got {events:?}"
        );
    }

    #[test]
    fn delete_sync_failure_prevents_ownership_finalization() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        let file = dir.join(CHANNEL_FILE_NAME);
        assert!(file.exists());

        barriers::reset();
        let failpoint = barriers::fail_sync_of(&dir);
        let report = reconcile(
            &PhpIniSettings::default(),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        drop(failpoint);

        assert!(
            matches!(outcome_for(&report, &file), WriteOutcome::Failed { .. }),
            "{report:?}"
        );
        let manifest = Manifest::load(&manifest_path).unwrap();
        assert!(
            manifest.files.iter().any(|e| e.path == file),
            "ownership must be retained until the removal is durable"
        );

        // Recovery (barrier restored) resolves the journaled delete.
        recover_pending(&manifest_path).unwrap();
        assert!(
            Manifest::load(&manifest_path).unwrap().files.is_empty(),
            "recovery finalizes the durable delete"
        );
    }

    #[test]
    fn unmanage_sync_failure_keeps_ownership() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");

        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &dir,
            )],
            &manifest_path,
            &roots,
        );
        let file = dir.join(CHANNEL_FILE_NAME);

        barriers::reset();
        let failpoint = barriers::fail_sync_of(&dir);
        let actions = unmanage(&manifest_path).unwrap();
        drop(failpoint);

        assert!(
            actions
                .iter()
                .any(|a| a.path == file && matches!(a.outcome, WriteOutcome::Failed { .. })),
            "{actions:?}"
        );
        assert!(
            Manifest::load(&manifest_path)
                .unwrap()
                .files
                .iter()
                .any(|e| e.path == file),
            "unmanage must not drop ownership past a failed removal barrier"
        );
    }

    #[test]
    fn config_sync_failure_prevents_legacy_rename() {
        let (_tmp, base) = canon_tmp();
        let php_dir = base.join("php");
        std::fs::create_dir_all(php_dir.join("8.4")).unwrap();
        std::fs::write(php_dir.join("8.4/php.ini"), "[php]\nmemory_limit=1G\n").unwrap();
        let config_dir = base.join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();

        barriers::reset();
        let failpoint = barriers::fail_sync_of(&config_dir);
        let mut config = crate::config::HearthConfig::default();
        let result = migrate_legacy_inis(&mut config, &config_dir.join("config.toml"), &php_dir);
        drop(failpoint);

        assert!(result.is_err(), "config barrier failure must propagate");
        assert!(
            php_dir.join("8.4/php.ini").exists(),
            "legacy source must be untouched when the canonical config is not durable"
        );
        assert!(!php_dir.join("8.4/php.ini.migrated.bak").exists());
    }

    #[test]
    fn legacy_rename_followed_by_source_dir_sync() {
        let (_tmp, base) = canon_tmp();
        let php_dir = base.join("php");
        let version_dir = php_dir.join("8.4");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(version_dir.join("php.ini"), "[php]\nmemory_limit=1G\n").unwrap();
        let config_dir = base.join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();

        barriers::reset();
        let mut config = crate::config::HearthConfig::default();
        migrate_legacy_inis(&mut config, &config_dir.join("config.toml"), &php_dir).unwrap();
        let events = barriers::events();
        barriers::reset();

        let config_sync = barriers::index_of(&events, &format!("sync:{}", config_dir.display()));
        let backup = version_dir.join("php.ini.migrated.bak");
        let backup_rename = barriers::index_of(&events, &format!("rename:{}", backup.display()));
        let source_sync = barriers::index_of(&events, &format!("sync:{}", version_dir.display()));
        assert!(
            config_sync < backup_rename && backup_rename < source_sync,
            "required order: config dir sync → legacy rename → source dir sync; got {events:?}"
        );
    }

    // ---- round-3: manifest-save failure propagation ----

    #[test]
    fn final_manifest_sync_failure_after_channel_write_reports_failed() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );

        // Attempt 1 = pending journal save (passes); attempt 2 = finalization.
        let updated = simple_ini("8.4", "memory_limit", "4G");
        let failpoint = barriers::fail_nth_sync_of(&manifest_dir, 2);
        let report = reconcile(
            &updated,
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        drop(failpoint);

        let file = dir.join(CHANNEL_FILE_NAME);
        match outcome_for(&report, &file) {
            WriteOutcome::Failed { error } => {
                assert!(
                    error.contains("finalization"),
                    "must carry the persistence error: {error}"
                );
            }
            other => panic!("finalization failure must never report success: {other:?}"),
        }
        // After a failed barrier the disk may expose EITHER the durable
        // pending journal (crash before rename) or the already-renamed final
        // manifest (rename landed, sync failed) — both must converge.
        let disk_state = Manifest::load(&manifest_path).unwrap().files[0].state;
        assert!(
            matches!(disk_state, EntryState::Pending | EntryState::Applied),
            "unexpected disk state {disk_state:?}"
        );

        // Clean retry converges from whichever state disk exposes.
        let report = reconcile(
            &updated,
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        assert!(
            matches!(outcome_for(&report, &file), WriteOutcome::Unchanged),
            "retry must converge: {report:?}"
        );
        assert_eq!(
            Manifest::load(&manifest_path).unwrap().files[0].state,
            EntryState::Applied
        );
    }

    #[test]
    fn final_manifest_sync_failure_after_delete_reports_failed_not_deleted() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        let file = dir.join(CHANNEL_FILE_NAME);

        // Attempt 1 = pending delete journal (passes); attempt 2 = ownership drop.
        let failpoint = barriers::fail_nth_sync_of(&manifest_dir, 2);
        let report = reconcile(
            &PhpIniSettings::default(),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        drop(failpoint);

        assert!(
            matches!(outcome_for(&report, &file), WriteOutcome::Failed { .. }),
            "a non-durable ownership drop must never report Deleted: {report:?}"
        );
        // Disk may expose the pending journal (rename not landed) or the
        // final ownerless manifest (rename landed, sync failed).
        let disk = Manifest::load(&manifest_path).unwrap();
        assert!(
            disk.files.is_empty() || disk.files[0].state == EntryState::Pending,
            "unexpected disk state: {disk:?}"
        );

        // Clean retry converges whether disk exposes pending or final state.
        let report = reconcile(
            &PhpIniSettings::default(),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        assert!(report.files.is_empty(), "{report:?}");
        assert!(Manifest::load(&manifest_path).unwrap().files.is_empty());
        assert!(!file.exists());
    }

    #[test]
    fn stale_prune_sync_failure_appears_in_report() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();

        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &dir,
            )],
            &manifest_path,
            &roots,
        );
        std::fs::remove_file(dir.join(CHANNEL_FILE_NAME)).unwrap();

        let failpoint = barriers::fail_sync_of(&manifest_dir);
        let report = reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[],
            &manifest_path,
            &roots,
        );
        drop(failpoint);

        assert!(
            report.files.iter().any(
                |f| matches!(&f.outcome, WriteOutcome::Failed { error } if error.contains("stale"))
            ),
            "a non-durable stale prune must be reported, not silent: {report:?}"
        );

        // Retry removes the stale entry durably.
        let report = reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[],
            &manifest_path,
            &roots,
        );
        assert!(report.files.is_empty(), "{report:?}");
        assert!(Manifest::load(&manifest_path).unwrap().files.is_empty());
    }

    #[test]
    fn missing_file_cleanup_sync_failure_appears_in_report() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        let file = dir.join(CHANNEL_FILE_NAME);
        std::fs::remove_file(&file).unwrap();

        // Delete flow, file already gone: ownership cleanup save fails.
        let failpoint = barriers::fail_sync_of(&manifest_dir);
        let report = reconcile(
            &PhpIniSettings::default(),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        drop(failpoint);

        assert!(
            matches!(outcome_for(&report, &file), WriteOutcome::Failed { .. }),
            "non-durable missing-file cleanup must be reported: {report:?}"
        );

        // Clean retry converges from whichever state disk exposes.
        let report = reconcile(
            &PhpIniSettings::default(),
            std::slice::from_ref(&target),
            &manifest_path,
            &roots,
        );
        assert!(report.files.is_empty(), "{report:?}");
        assert!(Manifest::load(&manifest_path).unwrap().files.is_empty());
    }

    #[test]
    fn refused_state_save_error_included_in_reason() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = roots.herd.join("config/php/84");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = base.join("hearth/php/manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();

        // Group-writable → verify_user_channel refuses after the journal save.
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o775);
        std::fs::set_permissions(&dir, perms).unwrap();

        // Attempt 1 = pending journal (passes); attempt 2 = refusal recording.
        let failpoint = barriers::fail_nth_sync_of(&manifest_dir, 2);
        let report = reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[verified_target(
                PhpProvider::Herd,
                "8.4",
                PhpSapi::Cli,
                &dir,
            )],
            &manifest_path,
            &roots,
        );
        drop(failpoint);

        let file = dir.join(CHANNEL_FILE_NAME);
        match outcome_for(&report, &file) {
            WriteOutcome::Refused { reason } => {
                assert!(reason.contains("channel blocked"), "{reason}");
                assert!(
                    reason.contains("additionally"),
                    "the failed state-recording save must not be lost: {reason}"
                );
            }
            other => panic!("expected Refused with appended save error, got {other:?}"),
        }

        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dir, perms).unwrap();
    }

    #[test]
    fn failed_state_save_error_included_in_reason() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = base.join("hearth/php/manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();

        // Channel barrier fails, then recording that failure also fails.
        let channel_fp = barriers::fail_sync_of(&dir);
        let manifest_fp = barriers::fail_nth_sync_of(&manifest_dir, 2);
        let report = reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &dir,
            )],
            &manifest_path,
            &roots,
        );
        drop(channel_fp);
        drop(manifest_fp);

        let file = dir.join(CHANNEL_FILE_NAME);
        match outcome_for(&report, &file) {
            WriteOutcome::Failed { error } => {
                assert!(
                    error.contains("additionally"),
                    "secondary persistence failure must be included: {error}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn unmanage_finalization_error_propagates() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();

        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &dir,
            )],
            &manifest_path,
            &roots,
        );

        let failpoint = barriers::fail_sync_of(&manifest_dir);
        let result = unmanage(&manifest_path);
        drop(failpoint);
        assert!(
            result.is_err(),
            "final manifest sync failure must propagate"
        );

        // Clean retry converges (files already durably removed).
        assert!(unmanage(&manifest_path).is_ok());
        assert!(Manifest::load(&manifest_path).unwrap().files.is_empty());
    }

    #[test]
    fn recovery_finalization_error_propagates() {
        let (_tmp, base) = canon_tmp();
        let dir = base.join("channel");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CHANNEL_FILE_NAME);
        let desired_content = "; managed by hearth\nmemory_limit=2G\n";
        std::fs::write(&file, desired_content).unwrap();
        let desired_sha = sha256_hex(desired_content.as_bytes());

        let manifest_path = base.join("manifest.toml");
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();
        pending_manifest(&manifest_path, &file, None, Some(&desired_sha));

        let failpoint = barriers::fail_sync_of(&manifest_dir);
        let result = recover_pending(&manifest_path);
        drop(failpoint);
        assert!(
            result.is_err(),
            "recovery finalization save failure must propagate"
        );

        // Clean retry converges: disk exposes pending (rename not landed —
        // recovery finalizes) or applied (rename landed — nothing pending).
        let actions = recover_pending(&manifest_path).unwrap();
        assert!(
            actions.is_empty() || matches!(actions[0].outcome, RecoveryOutcome::Finalized),
            "{actions:?}"
        );
        let manifest = Manifest::load(&manifest_path).unwrap();
        assert_eq!(manifest.files[0].state, EntryState::Applied);
    }

    #[test]
    fn no_ignored_manifest_save_results_in_source() {
        let source = include_str!("reconcile.rs");
        // Constructed at runtime so this test's own source never matches.
        let ignored = format!("let _ = {}.save", "manifest");
        assert!(
            !source.contains(&ignored),
            "every Manifest::save result must be handled — found an ignored one"
        );
    }

    // ---- migration ----

    #[test]
    fn migration_imports_legacy_ini_through_ini_guard() {
        let (_tmp, base) = canon_tmp();
        let php_dir = base.join("php");
        std::fs::create_dir_all(php_dir.join("8.4")).unwrap();
        std::fs::write(
            php_dir.join("8.4/php.ini"),
            "[php]\nmemory_limit=1G\nextension=evil.so\n",
        )
        .unwrap();

        let mut config = crate::config::HearthConfig::default();
        let config_path = base.join("config.toml");
        let report = migrate_legacy_inis(&mut config, &config_path, &php_dir).unwrap();

        assert_eq!(
            config
                .php_ini
                .overrides
                .get("8.4")
                .and_then(|m| m.get("memory_limit")),
            Some(&"1G".to_string())
        );
        assert!(
            config
                .php_ini
                .overrides
                .get("8.4")
                .map(|m| !m.contains_key("extension"))
                .unwrap_or(true),
            "denied directive must not be imported"
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].key, "extension");
        assert!(
            php_dir.join("8.4/php.ini.migrated.bak").exists(),
            "legacy file renamed to backup"
        );
        assert!(!php_dir.join("8.4/php.ini").exists());
        assert!(
            config_path.exists(),
            "config persisted to the injected path"
        );
    }

    #[test]
    fn migration_backup_no_clobber() {
        let (_tmp, base) = canon_tmp();
        let php_dir = base.join("php");
        std::fs::create_dir_all(php_dir.join("8.4")).unwrap();
        std::fs::write(php_dir.join("8.4/php.ini"), "[php]\nmemory_limit=1G\n").unwrap();
        // A previous backup already exists.
        std::fs::write(php_dir.join("8.4/php.ini.migrated.bak"), "old backup").unwrap();

        let mut config = crate::config::HearthConfig::default();
        migrate_legacy_inis(&mut config, &base.join("config.toml"), &php_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(php_dir.join("8.4/php.ini.migrated.bak")).unwrap(),
            "old backup",
            "existing backup must not be clobbered"
        );
        assert!(php_dir.join("8.4/php.ini.migrated.bak.1").exists());
    }

    #[test]
    fn migration_idempotent_second_run_noop() {
        let (_tmp, base) = canon_tmp();
        let php_dir = base.join("php");
        std::fs::create_dir_all(php_dir.join("8.4")).unwrap();
        std::fs::write(php_dir.join("8.4/php.ini"), "[php]\nmemory_limit=1G\n").unwrap();

        let mut config = crate::config::HearthConfig::default();
        let config_path = base.join("config.toml");
        let first = migrate_legacy_inis(&mut config, &config_path, &php_dir).unwrap();
        assert_eq!(first.migrated_files.len(), 1);

        // Override the imported value — the second run must not resurrect 1G.
        config
            .php_ini
            .overrides
            .get_mut("8.4")
            .unwrap()
            .insert("memory_limit".to_string(), "4G".to_string());

        let second = migrate_legacy_inis(&mut config, &config_path, &php_dir).unwrap();
        assert!(second.migrated_files.is_empty(), "{second:?}");
        assert!(second.skipped.is_empty());
        assert_eq!(
            config
                .php_ini
                .overrides
                .get("8.4")
                .and_then(|m| m.get("memory_limit")),
            Some(&"4G".to_string()),
            "second run must be a no-op"
        );
    }

    #[test]
    fn migration_save_failure_leaves_legacy_source_intact() {
        let (_tmp, base) = canon_tmp();
        let php_dir = base.join("php");
        std::fs::create_dir_all(php_dir.join("8.4")).unwrap();
        std::fs::write(php_dir.join("8.4/php.ini"), "[php]\nmemory_limit=1G\n").unwrap();

        // Force config persistence to fail: the config path's parent is a
        // regular file, so create_dir_all/save must error.
        std::fs::write(base.join("blocker"), "not a directory").unwrap();
        let bad_config_path = base.join("blocker/config.toml");

        let mut config = crate::config::HearthConfig::default();
        let result = migrate_legacy_inis(&mut config, &bad_config_path, &php_dir);
        assert!(result.is_err(), "save failure must surface as an error");

        // The only automatic import source must be untouched — no rename, no
        // backup, so a restart can still discover and import it.
        assert!(php_dir.join("8.4/php.ini").exists());
        assert!(!php_dir.join("8.4/php.ini.migrated.bak").exists());
        assert!(
            config.php_ini.overrides.is_empty(),
            "failed migration must not leave staged imports in the in-memory config"
        );

        // A fresh/restarted migration must succeed and import the value.
        let mut fresh = crate::config::HearthConfig::default();
        let config_path = base.join("config.toml");
        let report = migrate_legacy_inis(&mut fresh, &config_path, &php_dir).unwrap();
        assert_eq!(report.migrated_files.len(), 1);
        assert_eq!(
            fresh
                .php_ini
                .overrides
                .get("8.4")
                .and_then(|m| m.get("memory_limit")),
            Some(&"1G".to_string())
        );
        assert!(config_path.exists());
        assert!(!php_dir.join("8.4/php.ini").exists());
        assert!(php_dir.join("8.4/php.ini.migrated.bak").exists());
    }

    // ---- unmanage ----

    #[test]
    fn unmanage_resolves_journal_then_removes_only_tracked() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir_a = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let dir_b = roots.herd.join("config/php/85");
        std::fs::create_dir_all(&dir_b).unwrap();
        let manifest_path = base.join("hearth/php/manifest.toml");

        // Tracked file via real reconcile.
        reconcile(
            &simple_ini("8.4", "memory_limit", "2G"),
            &[verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &dir_a,
            )],
            &manifest_path,
            &roots,
        );
        let tracked = dir_a.join(CHANNEL_FILE_NAME);

        // Journaled-but-unfinalized file (crash after rename): unmanage must
        // resolve the journal first, then remove it like any tracked file.
        let pending_file = dir_b.join(CHANNEL_FILE_NAME);
        let pending_content = "; managed by hearth\nmemory_limit=3G\n";
        std::fs::write(&pending_file, pending_content).unwrap();
        let mut manifest = Manifest::load(&manifest_path).unwrap();
        manifest.files.push(ManifestEntry {
            path: pending_file.clone(),
            php_version: "8.5".to_string(),
            channel: "herd-user".to_string(),
            sha256: String::new(),
            state: EntryState::Pending,
            expected_old_sha256: None,
            desired_sha256: Some(sha256_hex(pending_content.as_bytes())),
            last_outcome: LastOutcome::Written,
        });
        manifest.save(&manifest_path).unwrap();

        // Modified (no longer hash-matching) tracked file must survive.
        std::fs::write(&tracked, "user edited this\n").unwrap();

        let actions = unmanage(&manifest_path).unwrap();

        assert!(!pending_file.exists(), "journal-resolved file removed");
        assert!(tracked.exists(), "hash-mismatched file survives");
        assert!(
            actions
                .iter()
                .any(|a| a.path == pending_file && matches!(a.outcome, WriteOutcome::Deleted))
        );
        assert!(
            actions
                .iter()
                .any(|a| a.path == tracked && matches!(a.outcome, WriteOutcome::Refused { .. }))
        );
    }

    // ---- B2-5 adversarial write-point checks (no create-before-verify) ----

    #[test]
    fn write_fails_typed_when_dir_disappears_after_classification_no_recreation() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let ini = simple_ini("8.4", "memory_limit", "2G");
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        // Adversary removes the classified/created dir before the write.
        std::fs::remove_dir_all(&dir).unwrap();

        let report = reconcile(&ini, &[target], &manifest_path, &roots);
        let file = dir.join(CHANNEL_FILE_NAME);
        match outcome_for(&report, &file) {
            WriteOutcome::Failed { error } => {
                assert!(error.contains("disappeared"), "got: {error}");
            }
            other => panic!("expected typed Failed, got {other:?}"),
        }
        assert!(!dir.exists(), "write path must NOT recreate the directory");
    }

    #[test]
    fn write_refused_when_ancestor_substituted_by_symlink_zero_outside_mutation() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("manifest-outside-hearth/manifest.toml");
        let ini = simple_ini("8.4", "memory_limit", "2G");
        let target = verified_target(PhpProvider::Hearth, "8.4", PhpSapi::Cli, &dir);

        // Adversary substitutes the `php` ancestor with a symlink escaping
        // into an attacker-controlled directory AFTER classification.
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("8.4/conf.d")).unwrap();
        let php_ancestor = base.join("hearth/php");
        std::fs::remove_dir_all(&php_ancestor).unwrap();
        std::os::unix::fs::symlink(&outside, &php_ancestor).unwrap();

        let report = reconcile(&ini, &[target], &manifest_path, &roots);
        let file = dir.join(CHANNEL_FILE_NAME);
        match outcome_for(&report, &file) {
            WriteOutcome::Refused { reason } => {
                assert!(reason.contains("symlink"), "got: {reason}");
            }
            other => panic!("expected typed Refused, got {other:?}"),
        }
        assert!(
            !outside.join("8.4/conf.d").join(CHANNEL_FILE_NAME).exists(),
            "zero mutation through the substituted ancestor"
        );
    }
}
