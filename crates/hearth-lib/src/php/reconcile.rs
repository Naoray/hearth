//! Materialization of the canonical PHP INI store into verified channel
//! files, owned by an authoritative manifest with a crash-safe pending
//! journal, plus guarded migration of legacy per-version Hearth INIs.
//!
//! Ownership rule (S4): overwrite/delete/unmanage require a manifest entry
//! whose recorded sha256 exactly matches the on-disk file. The rendered file
//! header is informational only — a headered file without a manifest entry is
//! a collision and is refused, never adopted.

use crate::config::{HearthConfig, PhpIniSettings};
use crate::php::targets::{
    PhpProvider, PhpTarget, ProviderRoots, ensure_hearth_channel_dir, verify_user_channel,
};
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// File name Hearth materializes into every verified channel. `zz-` sorts
/// last in the alphabetical scan order, so Hearth values win over Herd's
/// `php.ini` and Homebrew's `ext-*.ini`.
pub const CHANNEL_FILE_NAME: &str = "zz-hearth.ini";

pub const MANIFEST_VERSION: u64 = 1;

const DEFAULT_MANIFEST_LOCK_WAIT: Duration = Duration::from_secs(5);
const MANIFEST_LOCK_POLL: Duration = Duration::from_millis(25);

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
    /// Unix milliseconds when this entry last reached `Applied` via a real
    /// write (the materialization timestamp behind the truthful
    /// `pending restart` marker). `#[serde(default)]` — entries from older
    /// manifests carry `None` and never claim a pending restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at_unix_ms: Option<u64>,
}

/// Hearth-owned wall-clock in unix milliseconds; `None` if the clock is
/// before the epoch (never panics a reconcile).
fn now_unix_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
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

#[derive(Debug)]
pub(crate) struct ManifestLock {
    file: File,
    directory: GuardedManifestDir,
    path: PathBuf,
    dev: u64,
    ino: u64,
}

#[derive(Debug)]
struct GuardedManifestDir {
    file: File,
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl GuardedManifestDir {
    fn verify_identity(&self) -> Result<(), LockError> {
        let path_metadata = std::fs::symlink_metadata(&self.path)
            .map_err(|error| LockError::Io(error.to_string()))?;
        let fd_metadata = self
            .file
            .metadata()
            .map_err(|error| LockError::Io(error.to_string()))?;
        if path_metadata.file_type().is_symlink()
            || !path_metadata.file_type().is_dir()
            || path_metadata.dev() != self.dev
            || path_metadata.ino() != self.ino
            || fd_metadata.dev() != self.dev
            || fd_metadata.ino() != self.ino
        {
            return Err(LockError::Io(format!(
                "guarded manifest directory {} was replaced",
                self.path.display()
            )));
        }
        Ok(())
    }
}

impl ManifestLock {
    fn verify_identity(&self) -> Result<(), LockError> {
        self.directory.verify_identity()?;
        let path_metadata = std::fs::symlink_metadata(&self.path)
            .map_err(|error| LockError::Io(error.to_string()))?;
        let fd_metadata = self
            .file
            .metadata()
            .map_err(|error| LockError::Io(error.to_string()))?;
        if path_metadata.file_type().is_symlink()
            || !path_metadata.file_type().is_file()
            || path_metadata.dev() != self.dev
            || path_metadata.ino() != self.ino
            || fd_metadata.dev() != self.dev
            || fd_metadata.ino() != self.ino
            || fd_metadata.nlink() != 1
            || fd_metadata.len() != 0
        {
            return Err(LockError::Io(format!(
                "manifest lock {} has unsafe or replaced identity",
                self.path.display()
            )));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum LockError {
    Busy {
        manifest_dir: PathBuf,
        waited: Duration,
    },
    Io(String),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy {
                manifest_dir,
                waited,
            } => write!(
                f,
                "another Hearth process is updating the manifest in {} (waited {:?}) — retry",
                manifest_dir.display(),
                waited
            ),
            Self::Io(error) => write!(f, "manifest lock I/O failed: {error}"),
        }
    }
}

impl std::error::Error for LockError {}

pub(crate) fn acquire_manifest_lock(
    hearth_root: &Path,
    manifest_dir: &Path,
    wait: Duration,
) -> Result<ManifestLock, LockError> {
    let verified = ensure_hearth_channel_dir(hearth_root, manifest_dir)
        .map_err(|error| LockError::Io(error.to_string()))?;
    let directory_file =
        File::open(&verified.path).map_err(|error| LockError::Io(error.to_string()))?;
    let directory = GuardedManifestDir {
        file: directory_file,
        path: verified.path,
        dev: verified.dev,
        ino: verified.ino,
    };
    directory.verify_identity()?;

    let lock_path = directory.path.join(".manifest.lock");
    if let Ok(metadata) = std::fs::symlink_metadata(&lock_path)
        && (metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || metadata.nlink() != 1)
    {
        return Err(LockError::Io(format!(
            "manifest lock {} must be a single-link regular file",
            lock_path.display()
        )));
    }
    let existed = std::fs::symlink_metadata(&lock_path).is_ok();
    let raw_fd = openat(
        Some(directory.file.as_raw_fd()),
        Path::new(".manifest.lock"),
        OFlag::O_CLOEXEC | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_RDWR,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| LockError::Io(error.to_string()))?;
    // SAFETY: `openat` returned a new owned descriptor on success.
    let file = unsafe { File::from_raw_fd(raw_fd) };
    let metadata = file
        .metadata()
        .map_err(|error| LockError::Io(error.to_string()))?;
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.len() != 0
        || metadata.uid() != nix::unistd::geteuid().as_raw()
    {
        return Err(LockError::Io(format!(
            "manifest lock {} must be an owned, zero-byte, single-link regular file",
            lock_path.display()
        )));
    }
    let lock_dev = metadata.dev();
    let lock_ino = metadata.ino();
    let provisional = ManifestLock {
        file,
        directory,
        path: lock_path,
        dev: lock_dev,
        ino: lock_ino,
    };
    provisional.verify_identity()?;
    let started = Instant::now();
    loop {
        match provisional.file.try_lock() {
            Ok(()) => {
                provisional.verify_identity()?;
                let mode = provisional
                    .file
                    .metadata()
                    .map_err(|error| LockError::Io(error.to_string()))?
                    .permissions()
                    .mode()
                    & 0o777;
                if mode != 0o600 {
                    provisional
                        .file
                        .set_permissions(std::fs::Permissions::from_mode(0o600))
                        .map_err(|error| LockError::Io(error.to_string()))?;
                }
                provisional
                    .file
                    .sync_all()
                    .map_err(|error| LockError::Io(error.to_string()))?;
                if !existed {
                    provisional
                        .directory
                        .file
                        .sync_all()
                        .map_err(|error| LockError::Io(error.to_string()))?;
                }
                provisional.verify_identity()?;
                let effective_mode = provisional
                    .file
                    .metadata()
                    .map_err(|error| LockError::Io(error.to_string()))?
                    .permissions()
                    .mode()
                    & 0o777;
                if effective_mode != 0o600 {
                    return Err(LockError::Io(format!(
                        "manifest lock {} mode is {:04o}, expected 0600",
                        provisional.path.display(),
                        effective_mode
                    )));
                }
                return Ok(provisional);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if started.elapsed() >= wait {
                    return Err(LockError::Busy {
                        manifest_dir: manifest_dir.to_path_buf(),
                        waited: wait,
                    });
                }
                std::thread::sleep(MANIFEST_LOCK_POLL.min(wait.saturating_sub(started.elapsed())));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(LockError::Io(error.to_string()));
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum TxnError {
    Lock(LockError),
    Load(String),
    Recovery(String),
    Finalize(String),
}

impl fmt::Display for TxnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lock(error) => error.fmt(f),
            Self::Load(error) => write!(f, "manifest unreadable: {error}"),
            Self::Recovery(error) => write!(f, "journal recovery failed: {error}"),
            Self::Finalize(error) => write!(f, "manifest finalization failed: {error}"),
        }
    }
}

impl std::error::Error for TxnError {}

#[derive(Debug, Clone)]
pub(crate) struct OwnedArtifactSpec {
    pub final_path: PathBuf,
    pub channel: String,
    pub php_version: String,
    pub mode: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirIdentity {
    pub dev: u64,
    pub ino: u64,
}

pub(crate) trait ArtifactDirVerifier {
    fn verify(&self) -> Result<DirIdentity, String>;
    fn recheck(&self, id: &DirIdentity) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Consumed by the FPM transaction in the next staged commit.
pub(crate) enum FinalizeMode {
    SaveAlways,
    DeleteWhenEmpty,
}

pub(crate) struct ManifestTxn<'a> {
    manifest: &'a mut Manifest,
    manifest_path: &'a Path,
    finalize: FinalizeMode,
    finalize_requested: bool,
    recovery_actions: Option<Vec<RecoveryAction>>,
}

impl ManifestTxn<'_> {
    pub(crate) fn manifest(&mut self) -> &mut Manifest {
        self.manifest
    }

    pub(crate) fn apply_owned_artifact(
        &mut self,
        spec: &OwnedArtifactSpec,
        bytes: &[u8],
        verifier: &dyn ArtifactDirVerifier,
    ) -> WriteOutcome {
        let desired_sha = sha256_hex(bytes);
        let actual = file_sha256(&spec.final_path);

        if let Some(actual_sha) = &actual {
            match self.manifest.entry_index(&spec.final_path) {
                None => {
                    return WriteOutcome::Refused {
                        reason: format!(
                            "{} exists but is not tracked by the Hearth manifest — {}",
                            spec.final_path.display(),
                            REFUSED_REMEDIATION
                        ),
                    };
                }
                Some(idx) => {
                    let entry = &self.manifest.files[idx];
                    if !entry_owns(entry, actual_sha) {
                        return WriteOutcome::Refused {
                            reason: format!(
                                "{} was modified outside Hearth — {}",
                                spec.final_path.display(),
                                REFUSED_REMEDIATION
                            ),
                        };
                    }
                    if entry.sha256 == desired_sha
                        && entry.state == EntryState::Applied
                        && *actual_sha == desired_sha
                    {
                        return WriteOutcome::Unchanged;
                    }
                }
            }
        }

        let pending = ManifestEntry {
            path: spec.final_path.clone(),
            php_version: spec.php_version.clone(),
            channel: spec.channel.clone(),
            sha256: actual.clone().unwrap_or_default(),
            state: EntryState::Pending,
            expected_old_sha256: actual,
            desired_sha256: Some(desired_sha.clone()),
            last_outcome: LastOutcome::Written,
            applied_at_unix_ms: None,
        };
        match self.manifest.entry_index(&spec.final_path) {
            Some(idx) => self.manifest.files[idx] = pending,
            None => self.manifest.files.push(pending),
        }
        if let Err(error) = self.manifest.save(self.manifest_path) {
            return WriteOutcome::Failed {
                error: format!("could not journal pending write: {error}"),
            };
        }
        txn_test_pause(self.manifest_path, "after_journal");

        let dir = spec.final_path.parent().unwrap_or(Path::new("."));
        if !dir.exists() {
            return self.finalize_failure(format!(
                "channel directory {} disappeared after classification — refusing to recreate at write time; run `hearth php config --sync`",
                dir.display()
            ), &spec.final_path);
        }
        let verified = match verifier.verify() {
            Ok(identity) => identity,
            Err(rejection) => {
                let idx = self.manifest.entry_index(&spec.final_path).unwrap();
                self.manifest.files[idx].state = EntryState::Applied;
                self.manifest.files[idx].expected_old_sha256 = None;
                self.manifest.files[idx].desired_sha256 = None;
                self.manifest.files[idx].last_outcome = LastOutcome::Refused;
                let mut reason = format!("channel blocked: {rejection}");
                if let Err(error) = self.manifest.save(self.manifest_path) {
                    reason = format!(
                        "{reason}; additionally, recording this refusal in the manifest failed: {error}"
                    );
                }
                return WriteOutcome::Refused { reason };
            }
        };

        if let Err(error) = atomic_write(dir, &spec.final_path, bytes, spec.mode) {
            return self.finalize_failure(error.to_string(), &spec.final_path);
        }
        if let Err(error) = verifier.recheck(&verified) {
            return self.finalize_failure(error, &spec.final_path);
        }

        let idx = self.manifest.entry_index(&spec.final_path).unwrap();
        self.manifest.files[idx].sha256 = desired_sha;
        self.manifest.files[idx].state = EntryState::Applied;
        self.manifest.files[idx].expected_old_sha256 = None;
        self.manifest.files[idx].desired_sha256 = None;
        self.manifest.files[idx].last_outcome = LastOutcome::Written;
        self.manifest.files[idx].applied_at_unix_ms = now_unix_ms();
        if let Err(error) = self.manifest.save(self.manifest_path) {
            return WriteOutcome::Failed {
                error: format!("written but manifest finalization failed: {error}"),
            };
        }
        WriteOutcome::Written
    }

    pub(crate) fn remove_owned_artifact(&mut self, final_path: &Path) -> WriteOutcome {
        let actual = match file_sha256(final_path) {
            Some(sha) => sha,
            None => {
                if let Some(idx) = self.manifest.entry_index(final_path) {
                    let entry = self.manifest.files.remove(idx);
                    if let Err(error) = self.manifest.save(self.manifest_path) {
                        self.manifest.files.insert(idx, entry);
                        return WriteOutcome::Failed {
                            error: format!(
                                "could not durably release ownership of missing {}: {error}",
                                final_path.display()
                            ),
                        };
                    }
                }
                return WriteOutcome::Unchanged;
            }
        };

        let owned = self
            .manifest
            .entry_index(final_path)
            .map(|idx| entry_owns(&self.manifest.files[idx], &actual))
            .unwrap_or(false);
        if !owned {
            return WriteOutcome::Refused {
                reason: format!(
                    "{} is not tracked by the Hearth manifest — {}",
                    final_path.display(),
                    REFUSED_REMEDIATION
                ),
            };
        }

        let idx = self.manifest.entry_index(final_path).unwrap();
        self.manifest.files[idx].state = EntryState::Pending;
        self.manifest.files[idx].expected_old_sha256 = Some(actual);
        self.manifest.files[idx].desired_sha256 = None;
        if let Err(error) = self.manifest.save(self.manifest_path) {
            return WriteOutcome::Failed {
                error: format!("could not journal pending delete: {error}"),
            };
        }
        txn_test_pause(self.manifest_path, "after_journal");

        let removal = std::fs::remove_file(final_path).and_then(|()| {
            crate::fsync::note_remove(final_path);
            let parent = final_path.parent().unwrap_or(Path::new("."));
            crate::fsync::sync_dir(parent)
        });
        if let Err(error) = removal {
            return self.finalize_failure(error.to_string(), final_path);
        }
        let idx = self.manifest.entry_index(final_path).unwrap();
        let entry = self.manifest.files.remove(idx);
        if let Err(error) = self.manifest.save(self.manifest_path) {
            self.manifest.files.insert(idx, entry);
            return WriteOutcome::Failed {
                error: format!("channel file removed but manifest finalization failed: {error}"),
            };
        }
        WriteOutcome::Deleted
    }

    fn finalize_failure(&mut self, mut error: String, final_path: &Path) -> WriteOutcome {
        if let Some(idx) = self.manifest.entry_index(final_path) {
            self.manifest.files[idx].last_outcome = LastOutcome::Failed;
            if let Err(save_error) = self.manifest.save(self.manifest_path) {
                error = format!(
                    "{error}; additionally, recording this failure in the manifest failed: {save_error}"
                );
            }
        }
        WriteOutcome::Failed { error }
    }

    pub(crate) fn save_manifest(&mut self) -> anyhow::Result<()> {
        self.manifest.save(self.manifest_path)
    }

    pub(crate) fn request_finalize(&mut self) {
        self.finalize_requested = true;
    }

    fn take_recovery_actions(&mut self) -> Vec<RecoveryAction> {
        self.recovery_actions.take().unwrap_or_default()
    }

    fn finish(&mut self) -> Result<(), String> {
        match self.finalize {
            FinalizeMode::SaveAlways if self.finalize_requested => self
                .manifest
                .save(self.manifest_path)
                .map_err(|e| e.to_string()),
            FinalizeMode::DeleteWhenEmpty if self.manifest.files.is_empty() => {
                if self.manifest_path.exists() {
                    txn_test_pause(self.manifest_path, "before_manifest_unlink");
                    std::fs::remove_file(self.manifest_path).map_err(|e| e.to_string())?;
                    crate::fsync::note_remove(self.manifest_path);
                    let parent = self.manifest_path.parent().unwrap_or(Path::new("."));
                    crate::fsync::sync_dir(parent).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            FinalizeMode::DeleteWhenEmpty if self.finalize_requested => self
                .manifest
                .save(self.manifest_path)
                .map_err(|e| e.to_string()),
            _ => Ok(()),
        }
    }
}

pub(crate) fn manifest_set_transaction<T>(
    hearth_root: &Path,
    manifest_dir: &Path,
    manifest_path: &Path,
    lock_wait: Duration,
    loader: impl FnOnce(&Path) -> Result<Manifest, String>,
    finalize: FinalizeMode,
    body: impl FnOnce(&mut ManifestTxn<'_>) -> T,
) -> Result<T, TxnError> {
    let lock =
        acquire_manifest_lock(hearth_root, manifest_dir, lock_wait).map_err(TxnError::Lock)?;
    let _lock_file = &lock.file;
    lock.verify_identity().map_err(TxnError::Lock)?;
    let mut manifest = loader(manifest_path).map_err(TxnError::Load)?;
    lock.verify_identity().map_err(TxnError::Lock)?;
    let recovery_actions = recover_pending_manifest(&mut manifest, manifest_path)
        .map_err(|error| TxnError::Recovery(error.to_string()))?;
    lock.verify_identity().map_err(TxnError::Lock)?;
    let mut txn = ManifestTxn {
        manifest: &mut manifest,
        manifest_path,
        finalize,
        finalize_requested: false,
        recovery_actions: Some(recovery_actions),
    };
    let output = body(&mut txn);
    lock.verify_identity().map_err(TxnError::Lock)?;
    txn.finish().map_err(TxnError::Finalize)?;
    lock.verify_identity().map_err(TxnError::Lock)?;
    Ok(output)
}

struct UserChannelVerifier<'a> {
    dir: &'a Path,
    allowed: &'a [PathBuf],
}

impl ArtifactDirVerifier for UserChannelVerifier<'_> {
    fn verify(&self) -> Result<DirIdentity, String> {
        let verified = verify_user_channel(self.dir, self.allowed).map_err(|e| e.to_string())?;
        Ok(DirIdentity {
            dev: verified.dev,
            ino: verified.ino,
        })
    }

    fn recheck(&self, id: &DirIdentity) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(self.dir).map_err(|e| e.to_string())?;
        if metadata.dev() == id.dev && metadata.ino() == id.ino {
            Ok(())
        } else {
            Err("channel directory was replaced during the write".to_string())
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TxnTestPause {
    manifest_path: PathBuf,
    entered: PathBuf,
    release: PathBuf,
}

#[cfg(test)]
fn txn_test_pause_slot() -> &'static std::sync::Mutex<Option<TxnTestPause>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<TxnTestPause>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn configure_txn_test_pause(manifest_path: &Path, entered: &Path, release: &Path) {
    *txn_test_pause_slot().lock().unwrap() = Some(TxnTestPause {
        manifest_path: manifest_path.to_path_buf(),
        entered: entered.to_path_buf(),
        release: release.to_path_buf(),
    });
}

#[cfg(test)]
fn txn_test_pause(manifest_path: &Path, stage: &str) {
    let env_name = format!("HEARTH_TXN_PAUSE_{}", stage.to_ascii_uppercase());
    if let Some(prefix) = std::env::var_os(env_name) {
        let prefix = PathBuf::from(prefix);
        let entered = prefix.with_extension("entered");
        let release = prefix.with_extension("release");
        if let Some(parent) = entered.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&entered, b"entered").unwrap();
        if std::env::var_os("HEARTH_TXN_ABORT_AFTER_JOURNAL").is_some() {
            std::process::abort();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !release.exists() {
            assert!(
                Instant::now() < deadline,
                "transaction test pause timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    if stage != "after_journal" {
        return;
    }
    let pause = txn_test_pause_slot().lock().unwrap().clone();
    let Some(pause) = pause.filter(|pause| pause.manifest_path == manifest_path) else {
        return;
    };
    std::fs::write(&pause.entered, b"entered").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pause.release.exists() {
        assert!(
            Instant::now() < deadline,
            "transaction test pause timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    *txn_test_pause_slot().lock().unwrap() = None;
}

#[cfg(not(test))]
fn txn_test_pause(_manifest_path: &Path, _stage: &str) {}

/// Resolve every pending journal entry deterministically. Idempotent; called
/// on daemon boot (before any reconcile) and before unmanage.
pub fn recover_pending(
    manifest_path: &Path,
    hearth_root: &Path,
) -> anyhow::Result<Vec<RecoveryAction>> {
    let manifest_dir = manifest_path.parent().unwrap_or(Path::new("."));
    manifest_set_transaction(
        hearth_root,
        manifest_dir,
        manifest_path,
        DEFAULT_MANIFEST_LOCK_WAIT,
        |path| Manifest::load(path).map_err(|e| e.to_string()),
        FinalizeMode::SaveAlways,
        |txn| txn.take_recovery_actions(),
    )
    .map_err(anyhow::Error::from)
}

fn recover_pending_manifest(
    manifest: &mut Manifest,
    manifest_path: &Path,
) -> anyhow::Result<Vec<RecoveryAction>> {
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
                entry.applied_at_unix_ms = now_unix_ms();
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
    let manifest_dir = manifest_path.parent().unwrap_or(Path::new("."));
    match manifest_set_transaction(
        &roots.hearth,
        manifest_dir,
        manifest_path,
        DEFAULT_MANIFEST_LOCK_WAIT,
        |path| Manifest::load(path).map_err(|error| error.to_string()),
        FinalizeMode::SaveAlways,
        |txn| {
            let mut report = ReconcileReport::default();
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
            let mut produced = Vec::new();
            for ((version, dir), provider) in groups {
                let file_path = dir.join(CHANNEL_FILE_NAME);
                produced.push(file_path.clone());
                let effective = php_ini.effective_for(&version);
                let kind = channel_kind(provider);
                let outcome = if effective.is_empty() {
                    let outcome = txn.remove_owned_artifact(&file_path);
                    (!matches!(outcome, WriteOutcome::Unchanged)).then_some(outcome)
                } else {
                    let content = render_ini(&effective);
                    let spec = OwnedArtifactSpec {
                        final_path: file_path.clone(),
                        channel: kind.to_string(),
                        php_version: version.clone(),
                        mode: 0o644,
                    };
                    let verifier = UserChannelVerifier {
                        dir: &dir,
                        allowed: &allowed,
                    };
                    Some(txn.apply_owned_artifact(&spec, content.as_bytes(), &verifier))
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

            let changed = {
                let manifest = txn.manifest();
                let before = manifest.files.len();
                manifest
                    .files
                    .retain(|entry| produced.contains(&entry.path) || entry.path.exists());
                manifest.files.len() != before
            };
            if changed && let Err(error) = txn.save_manifest() {
                report.files.push(FileAction {
                    path: manifest_path.to_path_buf(),
                    php_version: String::new(),
                    channel: String::new(),
                    outcome: WriteOutcome::Failed {
                        error: format!(
                            "stale manifest entries could not be pruned durably: {error}"
                        ),
                    },
                });
            }
            report
        },
    ) {
        Ok(report) => report,
        Err(error) => ReconcileReport {
            files: vec![FileAction {
                path: manifest_path.to_path_buf(),
                php_version: String::new(),
                channel: String::new(),
                outcome: WriteOutcome::Failed {
                    error: error.to_string(),
                },
            }],
            skipped: Vec::new(),
        },
    }
}

/// Remove every Hearth-written channel file (journal-resolved, exact-hash
/// deletions only). Hash-mismatched survivors are reported Refused.
pub fn unmanage(manifest_path: &Path, hearth_root: &Path) -> anyhow::Result<Vec<FileAction>> {
    let manifest_dir = manifest_path.parent().unwrap_or(Path::new("."));
    manifest_set_transaction(
        hearth_root,
        manifest_dir,
        manifest_path,
        DEFAULT_MANIFEST_LOCK_WAIT,
        |path| Manifest::load(path).map_err(|error| error.to_string()),
        FinalizeMode::SaveAlways,
        |txn| {
            let entries = txn.manifest().files.clone();
            let mut actions = Vec::new();
            for entry in entries {
                let outcome = txn.remove_owned_artifact(&entry.path);
                if let WriteOutcome::Failed { error } = &outcome
                    && (error.starts_with("could not journal pending delete:")
                        || error
                            .starts_with("channel file removed but manifest finalization failed:"))
                {
                    return Err(error.clone());
                }
                if !matches!(outcome, WriteOutcome::Unchanged) {
                    actions.push(FileAction {
                        path: entry.path,
                        php_version: entry.php_version,
                        channel: entry.channel,
                        outcome,
                    });
                }
            }
            txn.request_finalize();
            Ok(actions)
        },
    )
    .map_err(anyhow::Error::from)?
    .map_err(anyhow::Error::msg)
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    fn canon_tmp() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = tmp.path().canonicalize().unwrap();
        (tmp, canonical)
    }

    fn test_roots(base: &Path) -> ProviderRoots {
        let roots = ProviderRoots::isolated(
            base,
            base.join("hearth"),
            base.join("Herd"),
            base.join("homebrew"),
        )
        .unwrap();
        std::fs::create_dir_all(&roots.hearth).unwrap();
        roots
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
            identity_verified: true,
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
            identity_verified: true,
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
                applied_at_unix_ms: None,
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

        let actions = recover_pending(&manifest_path, manifest_path.parent().unwrap()).unwrap();
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

        let actions = recover_pending(&manifest_path, manifest_path.parent().unwrap()).unwrap();
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

        let actions = recover_pending(&manifest_path, manifest_path.parent().unwrap()).unwrap();
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
        let actions = recover_pending(&manifest_path, manifest_path.parent().unwrap()).unwrap();
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
        recover_pending(&manifest_path, manifest_path.parent().unwrap()).unwrap();
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
        let actions = unmanage(&manifest_path, manifest_path.parent().unwrap()).unwrap();
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
        let result = unmanage(&manifest_path, manifest_path.parent().unwrap());
        drop(failpoint);
        assert!(
            result.is_err(),
            "final manifest sync failure must propagate"
        );

        // Clean retry converges (files already durably removed).
        assert!(unmanage(&manifest_path, manifest_path.parent().unwrap()).is_ok());
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
        let result = recover_pending(&manifest_path, manifest_path.parent().unwrap());
        drop(failpoint);
        assert!(
            result.is_err(),
            "recovery finalization save failure must propagate"
        );

        // Clean retry converges: disk exposes pending (rename not landed —
        // recovery finalizes) or applied (rename landed — nothing pending).
        let actions = recover_pending(&manifest_path, manifest_path.parent().unwrap()).unwrap();
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
            applied_at_unix_ms: None,
        });
        manifest.save(&manifest_path).unwrap();

        // Modified (no longer hash-matching) tracked file must survive.
        std::fs::write(&tracked, "user edited this\n").unwrap();

        let actions = unmanage(&manifest_path, manifest_path.parent().unwrap()).unwrap();

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
        let manifest_path = roots.hearth.join("manifest-store/manifest.toml");
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

    #[derive(Clone)]
    struct RecordingVerifier {
        dir: PathBuf,
        manifest_path: PathBuf,
        calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl ArtifactDirVerifier for RecordingVerifier {
        fn verify(&self) -> Result<DirIdentity, String> {
            let manifest = Manifest::load(&self.manifest_path).map_err(|e| e.to_string())?;
            assert_eq!(manifest.files.len(), 1);
            assert_eq!(manifest.files[0].state, EntryState::Pending);
            self.calls.lock().unwrap().push("verify-after-journal");
            let meta = std::fs::metadata(&self.dir).map_err(|e| e.to_string())?;
            use std::os::unix::fs::MetadataExt;
            Ok(DirIdentity {
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }

        fn recheck(&self, id: &DirIdentity) -> Result<(), String> {
            let manifest = Manifest::load(&self.manifest_path).map_err(|e| e.to_string())?;
            assert_eq!(manifest.files[0].state, EntryState::Pending);
            self.calls.lock().unwrap().push("recheck-after-write");
            let meta = std::fs::metadata(&self.dir).map_err(|e| e.to_string())?;
            use std::os::unix::fs::MetadataExt;
            if meta.dev() == id.dev && meta.ino() == id.ino {
                Ok(())
            } else {
                Err("directory identity changed".to_string())
            }
        }
    }

    #[test]
    fn apply_owned_artifact_matches_reconcile_journal_ordering() {
        let (_tmp, base) = canon_tmp();
        let manifest_dir = base.join("manifest");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        let manifest_path = manifest_dir.join("manifest.toml");
        let artifact = manifest_dir.join("owned.conf");
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let verifier = RecordingVerifier {
            dir: manifest_dir.clone(),
            manifest_path: manifest_path.clone(),
            calls: calls.clone(),
        };
        let spec = OwnedArtifactSpec {
            final_path: artifact.clone(),
            channel: "test".to_string(),
            php_version: "8.4".to_string(),
            mode: 0o600,
        };

        let outcome = manifest_set_transaction(
            &manifest_dir,
            &manifest_dir,
            &manifest_path,
            std::time::Duration::from_secs(1),
            |path| Manifest::load(path).map_err(|error| error.to_string()),
            FinalizeMode::SaveAlways,
            |txn| txn.apply_owned_artifact(&spec, b"first\n", &verifier),
        )
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["verify-after-journal", "recheck-after-write"]
        );
        let manifest = Manifest::load(&manifest_path).unwrap();
        assert_eq!(manifest.files[0].state, EntryState::Applied);
        assert!(manifest.files[0].applied_at_unix_ms.is_some());

        std::fs::write(&artifact, "tampered\n").unwrap();
        let refused = manifest_set_transaction(
            &manifest_dir,
            &manifest_dir,
            &manifest_path,
            std::time::Duration::from_secs(1),
            |path| Manifest::load(path).map_err(|error| error.to_string()),
            FinalizeMode::SaveAlways,
            |txn| txn.apply_owned_artifact(&spec, b"second\n", &verifier),
        )
        .unwrap();
        assert!(matches!(refused, WriteOutcome::Refused { .. }));
        assert_eq!(std::fs::read(&artifact).unwrap(), b"tampered\n");
    }

    #[test]
    fn manifest_lock_child_role() {
        let Some(dir) = std::env::var_os("HEARTH_MANIFEST_LOCK_CHILD_DIR") else {
            return;
        };
        let expect_busy = std::env::var_os("HEARTH_MANIFEST_LOCK_EXPECT_BUSY").is_some();
        let result = acquire_manifest_lock(
            Path::new(&dir),
            Path::new(&dir),
            std::time::Duration::from_millis(100),
        );
        if expect_busy {
            assert!(matches!(result, Err(LockError::Busy { .. })));
        } else {
            assert!(
                result.is_ok(),
                "child failed to acquire released lock: {result:?}"
            );
        }
    }

    fn run_lock_child(dir: &Path, expect_busy: bool) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("manifest_lock_child_role")
            .arg("--nocapture")
            .env("HEARTH_MANIFEST_LOCK_CHILD_DIR", dir);
        if expect_busy {
            command.env("HEARTH_MANIFEST_LOCK_EXPECT_BUSY", "1");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "lock child failed: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn manifest_lock_excludes_second_acquirer() {
        let (_tmp, base) = canon_tmp();
        let manifest_dir = base.join("locked");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        let first = acquire_manifest_lock(
            &manifest_dir,
            &manifest_dir,
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        run_lock_child(&manifest_dir, true);
        drop(first);
        run_lock_child(&manifest_dir, false);

        let lock_path = manifest_dir.join(".manifest.lock");
        assert!(lock_path.is_file());
        assert_eq!(std::fs::metadata(&lock_path).unwrap().len(), 0);
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn manifest_lock_rejects_ambiguous_identity_and_enforces_zero_byte_mode() {
        let (_tmp, base) = canon_tmp();
        let manifest_dir = base.join("locked");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        let lock_path = manifest_dir.join(".manifest.lock");
        let sentinel = base.join("outside-sentinel");
        std::fs::write(&sentinel, b"outside-bytes").unwrap();
        std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o640)).unwrap();
        let sentinel_before = std::fs::metadata(&sentinel).unwrap();

        std::os::unix::fs::symlink(&sentinel, &lock_path).unwrap();
        assert!(acquire_manifest_lock(&base, &manifest_dir, Duration::ZERO).is_err());
        std::fs::remove_file(&lock_path).unwrap();

        std::fs::create_dir(&lock_path).unwrap();
        assert!(acquire_manifest_lock(&base, &manifest_dir, Duration::ZERO).is_err());
        std::fs::remove_dir(&lock_path).unwrap();

        std::fs::hard_link(&sentinel, &lock_path).unwrap();
        assert!(acquire_manifest_lock(&base, &manifest_dir, Duration::ZERO).is_err());
        std::fs::remove_file(&lock_path).unwrap();

        std::fs::write(&lock_path, []).unwrap();
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let lock = acquire_manifest_lock(&base, &manifest_dir, Duration::ZERO).unwrap();
        assert_eq!(
            lock.file.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(lock);
        std::fs::remove_file(&lock_path).unwrap();

        std::fs::write(&lock_path, b"must-not-truncate").unwrap();
        assert!(acquire_manifest_lock(&base, &manifest_dir, Duration::ZERO).is_err());
        assert_eq!(std::fs::read(&lock_path).unwrap(), b"must-not-truncate");

        let sentinel_after = std::fs::metadata(&sentinel).unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside-bytes");
        assert_eq!(sentinel_after.ino(), sentinel_before.ino());
        assert_eq!(sentinel_after.permissions().mode() & 0o777, 0o640);
    }

    #[test]
    fn ini_recovery_and_reconcile_reject_symlinked_manifest_dir_zero_outside_mutation() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        std::fs::create_dir_all(&roots.hearth).unwrap();
        let outside = base.join("outside-ini");
        std::fs::create_dir_all(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, b"outside-ini-bytes").unwrap();
        let sentinel_before = std::fs::metadata(&sentinel).unwrap();
        std::os::unix::fs::symlink(&outside, roots.hearth.join("php")).unwrap();
        let manifest_path = roots.hearth.join("php/manifest.toml");

        let recovery = recover_pending(&manifest_path, &roots.hearth);
        assert!(recovery.is_err());
        let report = reconcile(&PhpIniSettings::default(), &[], &manifest_path, &roots);
        assert!(matches!(
            report.files.as_slice(),
            [FileAction {
                outcome: WriteOutcome::Failed { .. },
                ..
            }]
        ));

        let sentinel_after = std::fs::metadata(&sentinel).unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside-ini-bytes");
        assert_eq!(sentinel_after.ino(), sentinel_before.ino());
        assert!(!outside.join(".manifest.lock").exists());
        assert!(!outside.join("manifest.toml").exists());
    }

    fn wait_for_file(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn ini_reconcile_acquires_and_releases_the_manifest_lock() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = expected_channel_dir(PhpProvider::Hearth, "8.4", &roots);
        let manifest_path = base.join("hearth/php/manifest.toml");
        let entered = base.join("barrier/entered");
        let release = base.join("barrier/release");
        std::fs::create_dir_all(entered.parent().unwrap()).unwrap();
        configure_txn_test_pause(&manifest_path, &entered, &release);

        let thread_manifest = manifest_path.clone();
        let thread_roots = roots.clone();
        let handle = std::thread::spawn(move || {
            let target = verified_target(
                PhpProvider::Hearth,
                "8.4",
                PhpSapi::Cli,
                &expected_channel_dir(PhpProvider::Hearth, "8.4", &thread_roots),
            );
            reconcile(
                &simple_ini("8.4", "memory_limit", "2G"),
                &[target],
                &thread_manifest,
                &thread_roots,
            )
        });

        wait_for_file(&entered);
        run_lock_child(manifest_path.parent().unwrap(), true);
        std::fs::write(&release, b"go").unwrap();
        let report = handle.join().unwrap();
        let file = dir.join(CHANNEL_FILE_NAME);
        assert_eq!(outcome_for(&report, &file), &WriteOutcome::Written);
        run_lock_child(manifest_path.parent().unwrap(), false);
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            render_ini(&simple_ini("8.4", "memory_limit", "2G").effective_for("8.4"))
        );
    }

    #[test]
    fn finalize_mode_save_always_preserves_empty_ini_manifest() {
        let (_tmp, base) = canon_tmp();
        let manifest_path = base.join("php/manifest.toml");
        let actions = unmanage(&manifest_path, &base).unwrap();
        assert!(actions.is_empty());
        assert!(manifest_path.is_file());
        assert!(Manifest::load(&manifest_path).unwrap().files.is_empty());
    }
}
