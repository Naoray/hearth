//! Provider-explicit PHP target discovery, SAPI-aware scan-dir probing, and
//! write-channel classification.
//!
//! Identity is `(provider, version, sapi, canonical_binary)` — a same-version
//! binary from one provider NEVER hides another provider's (Herd php85 and
//! Homebrew php@8.5 are co-installed realities, not duplicates).
//!
//! Probing runs the real binary (`--ini` for CLI, `-i` for FPM — php-fpm has
//! no `--ini`) in two env contexts plus an env-honor canary. The canary is
//! load-bearing because scan behavior is launcher-environment-dependent:
//! machine ground truth (2026-07-18, exact binary hashes in PR #26) shows
//! Herd-patched binaries ignore `PHP_INI_SCAN_DIR` whenever their own
//! `HERD_PHP_XY_INI_SCAN_DIR` variable is present (Herd's shell integration
//! exports it), and honor it — including leading-colon append, marker parsed
//! last and winning — when that variable is absent, in which case their
//! compiled default scan dir is the privileged `/usr/local/etc/php/conf.d`.
//! `HOME` is NOT the trigger. Classification therefore never assumes
//! provider folklore; it reports whatever the probes observe in the actual
//! launch context.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhpProvider {
    Hearth,
    Herd,
    Homebrew,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhpSapi {
    Cli,
    Fpm,
}

/// Stable identity — provider-explicit, NEVER cross-provider deduped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpTargetIdentity {
    pub provider: PhpProvider,
    pub version: String,
    pub sapi: PhpSapi,
    /// Canonicalized binary path.
    pub binary: PathBuf,
}

/// Scan-dir probe results for one binary.
#[derive(Debug, Clone, Default)]
pub struct ProbeInfo {
    /// Daemon env minus `PHP_INI_SCAN_DIR`/`PHPRC` (HOME present).
    pub normal_scan_dir: Option<PathBuf>,
    /// `env_clear` + `PATH=/usr/bin:/bin`.
    pub sanitized_scan_dir: Option<PathBuf>,
    /// Env-honor canary result: does the binary FUNCTIONALLY honor
    /// `PHP_INI_SCAN_DIR` under the normal (daemon) environment — marker ini
    /// actually parsed? Herd-patched binaries do not when their
    /// `HERD_PHP_XY_INI_SCAN_DIR` launcher variable is present.
    pub env_honored: bool,
    /// Probe failures (timeout, spawn error) — classification degrades these
    /// to BestEffort instead of guessing.
    pub failures: Vec<String>,
}

/// Write-side channel classification per (target, context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelClass {
    /// User-owned, version-exclusive, verified channel loaded in this context.
    Verified { dir: PathBuf },
    /// Only scan channel is a shared root-owned dir — NEVER written (option A).
    PrivilegedDir { dir: PathBuf },
    /// Probe failed / unknown layout / verification failed — reported, no writes.
    BestEffort { reason: String },
}

// Round-4 locked design (review 5594 rev4, brief 5625): ambient/external FPM
// observation can NEVER produce verified evidence, a managed row, or a write
// target in Stage B — the daemon's own inherited shell environment was never
// evidence for an external process, and no observable launch context is
// authenticatable from outside. The former `ExternalFpmEvidence`/
// `VerifiedExternalFpm` types, the process detector, and the evaluator were
// DELETED so that argv masquerade, mutable titles, ambiguous executable
// records, duplicate candidates, PID reuse, and stale evidence cannot grant
// authority by construction — there is no ambient authority source to attack.
// Authoritative Hearth-generated FPM ownership is todo #2343.

/// A discovered PHP target with probe results and per-context classification.
#[derive(Debug, Clone)]
pub struct PhpTarget {
    pub id: PhpTargetIdentity,
    pub probe: ProbeInfo,
    pub normal_channel: ChannelClass,
    pub sanitized_channel: ChannelClass,
    /// `Some` iff a Verified channel may receive `zz-hearth.ini`. Always
    /// `None` for ambient-provider FPM targets (round-4 locked design:
    /// external FPM never authorizes writes in Stage B).
    pub write_channel: Option<PathBuf>,
}

/// Injected provider base directories. Production uses the real roots
/// (`config_dir()`, `~/Library/Application Support/Herd`, `/opt/homebrew`);
/// tests use tempdirs. The allowlist for channel verification derives from
/// these roots, all of which live under `$HOME` or `/opt/homebrew` in
/// production.
#[derive(Debug, Clone)]
pub struct ProviderRoots {
    /// Hearth config dir (contains `php/{v}/…`).
    pub hearth: PathBuf,
    /// Herd base dir (contains `bin/php{XY}` and `config/php/{XY}`).
    pub herd: PathBuf,
    /// Homebrew prefix (contains `opt/php@{v}/…` and `etc/php/{v}/conf.d`).
    pub homebrew: PathBuf,
    /// Immutable allowed WRITE roots (review 5594 B2-4). Discovery roots
    /// NEVER imply write authority: production is fixed to the Hearth config
    /// dir, the user's home scope, and `/opt/homebrew`; an explicitly
    /// constructed isolated runtime is confined to its runtime root. Not
    /// derivable from lone environment overrides.
    allowed_write_roots: Vec<PathBuf>,
}

impl ProviderRoots {
    /// Runtime detection with fail-closed override semantics (B2-4):
    ///
    /// - No overrides → production: real Hearth/Herd/Homebrew discovery
    ///   roots and the IMMUTABLE production write allowlist (Hearth config
    ///   dir, user home scope, `/opt/homebrew`).
    /// - `HEARTH_ISOLATED_ROOT` set → typed isolated mode via
    ///   [`ProviderRoots::isolated`]: `HEARTH_CONFIG_DIR` and any provider
    ///   overrides must live beneath that root; write authority is confined
    ///   to it.
    /// - A lone `HEARTH_HERD_ROOT`/`HEARTH_HOMEBREW_ROOT` without
    ///   `HEARTH_ISOLATED_ROOT` is an error — a discovery override can never
    ///   silently alter production behavior or enlarge write authority.
    pub fn detect() -> Result<Self, String> {
        Self::detect_with_home(dirs::home_dir())
    }

    /// Injectable home resolution (review 5594 B3-2): HOME-unavailable
    /// behavior is deterministic and fail-closed — there is NO fallback root.
    pub fn detect_with_home(home: Option<PathBuf>) -> Result<Self, String> {
        // Distinguish "absent" from "present but invalid": every PRESENT
        // override is validated; empty/relative values are errors, never
        // silently unset/defaulted.
        let read_override = |name: &str| -> Result<Option<PathBuf>, String> {
            match std::env::var_os(name) {
                None => Ok(None),
                Some(v) if v.is_empty() => Err(format!(
                    "{name} is set but empty — unset it or point it at an absolute path"
                )),
                Some(v) => {
                    let path = PathBuf::from(v);
                    if !path.is_absolute() {
                        return Err(format!(
                            "{name} must be an absolute path, got {}",
                            path.display()
                        ));
                    }
                    Ok(Some(path))
                }
            }
        };
        let herd_override = read_override("HEARTH_HERD_ROOT")?;
        let brew_override = read_override("HEARTH_HOMEBREW_ROOT")?;

        match std::env::var_os("HEARTH_ISOLATED_ROOT") {
            Some(root) => {
                if root.is_empty() {
                    return Err("HEARTH_ISOLATED_ROOT is set but empty".to_string());
                }
                let root = PathBuf::from(root);
                if !root.is_absolute() {
                    return Err(format!(
                        "HEARTH_ISOLATED_ROOT must be absolute, got {}",
                        root.display()
                    ));
                }
                let hearth = crate::config_dir();
                let herd = herd_override.unwrap_or_else(|| root.join("herd"));
                let homebrew = brew_override.unwrap_or_else(|| root.join("homebrew"));
                Self::isolated(&root, hearth, herd, homebrew)
            }
            None => {
                if herd_override.is_some() || brew_override.is_some() {
                    return Err(
                        "HEARTH_HERD_ROOT/HEARTH_HOMEBREW_ROOT are test/smoke discovery \
                         overrides and require HEARTH_ISOLATED_ROOT — refusing to alter \
                         production provider discovery or write authority"
                            .to_string(),
                    );
                }
                let home = home.ok_or_else(|| {
                    "cannot resolve the user home directory — refusing to construct a \
                     production write allowlist (fail closed)"
                        .to_string()
                })?;
                Self::production_checked(home)
            }
        }
    }

    fn production_checked(home: PathBuf) -> Result<Self, String> {
        // B4-2: canonicalize-and-validate FIRST. A HOME that is missing, a
        // file, `/`, or a symlink alias of `/`/denylisted roots fails closed
        // with no allowlist constructed.
        let home = canonical_allowed_root("home", &home).map_err(|e| {
            format!("{e} — refusing to construct a production write allowlist (fail closed)")
        })?;
        let hearth = crate::config_dir();
        let hearth_allowed = canonical_allowed_root_allowing_missing("hearth config", &hearth)?;
        let mut allowed_write_roots = vec![hearth_allowed, home.clone()];
        // Homebrew authority only when the canonical /opt/homebrew scope
        // actually exists; its absence removes authority, never widens it.
        if let Ok(brew) = canonical_allowed_root("homebrew", Path::new("/opt/homebrew")) {
            allowed_write_roots.push(brew);
        }
        Ok(Self {
            // Provider discovery roots derive from the validated canonical
            // HOME boundary.
            herd: home.join("Library/Application Support/Herd"),
            homebrew: PathBuf::from("/opt/homebrew"),
            allowed_write_roots,
            hearth,
        })
    }

    /// Explicit typed isolated-runtime constructor (tests/smoke). Every root
    /// must be non-empty, absolute, outside the privileged denylist, and
    /// contained beneath `runtime_root`; write authority is exactly
    /// `runtime_root` and nothing else. Invalid inputs fail with actionable
    /// errors — never a silent fallback.
    pub fn isolated(
        runtime_root: &Path,
        hearth: PathBuf,
        herd: PathBuf,
        homebrew: PathBuf,
    ) -> Result<Self, String> {
        // B4-2: the test-only constructor keeps the FULL invariant — the
        // runtime root must canonicalize to an existing directory that is
        // neither `/` (nor an alias of it) nor denylisted.
        let canonical_root = canonical_allowed_root("isolated runtime", runtime_root)?;
        for (name, dir) in [
            ("hearth", &hearth),
            ("herd", &herd),
            ("homebrew", &homebrew),
        ] {
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                return Err(format!(
                    "isolated {name} root must be a non-empty absolute path, got {}",
                    dir.display()
                ));
            }
            // Containment check on the canonical form of the deepest
            // existing ancestor (roots may not exist yet).
            let mut probe = dir.clone();
            while !probe.exists() {
                probe = match probe.parent() {
                    Some(p) => p.to_path_buf(),
                    None => break,
                };
            }
            let canonical = probe.canonicalize().unwrap_or(probe);
            if !canonical.starts_with(&canonical_root) {
                return Err(format!(
                    "isolated {name} root {} is not contained beneath the isolated \
                     runtime root {}",
                    dir.display(),
                    canonical_root.display()
                ));
            }
            if canonical.starts_with(PRIVILEGED_PREFIX) {
                return Err(format!(
                    "isolated {name} root {} is under {PRIVILEGED_PREFIX}",
                    dir.display()
                ));
            }
        }
        Ok(Self {
            hearth,
            herd,
            homebrew,
            allowed_write_roots: vec![canonical_root],
        })
    }

    /// Canonicalized allowed WRITE prefixes. Immutable per construction:
    /// production = Hearth config dir + `$HOME` + `/opt/homebrew`; isolated
    /// = the isolated runtime root only. Discovery-root overrides never
    /// appear here.
    pub fn allowed_prefixes(&self) -> Vec<PathBuf> {
        // Stored roots are already canonical-validated at construction
        // (B4-2); no later canonicalization may expand authority here.
        self.allowed_write_roots.clone()
    }
}

/// Shared root-owned, version-blind fallback scan dir — hard-denylisted:
/// never written, never verified, always `PrivilegedDir`.
const PRIVILEGED_PREFIX: &str = "/usr/local/etc/php";

/// Canonical allowed-root invariant (review 5594 B4-2) — the ONE constructor
/// every allowed write prefix goes through: the path must be non-empty,
/// absolute, canonicalize to an EXISTING directory, and that canonical form
/// must be neither `/` nor under the privileged denylist. Symlink aliases
/// are resolved BEFORE the checks, so an alias of `/` or a denylisted root
/// can never slip through. Errors propagate — no permissive fallback.
fn canonical_allowed_root(name: &str, path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(format!(
            "{name} root must be a non-empty absolute path, got {}",
            path.display()
        ));
    }
    let canonical = path.canonicalize().map_err(|e| {
        format!(
            "{name} root {} must exist and canonicalize: {e}",
            path.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "{name} root {} is not a directory",
            canonical.display()
        ));
    }
    if canonical == Path::new("/") {
        return Err(format!(
            "{name} root {} resolves to `/` — `/` can never be write authority",
            path.display()
        ));
    }
    if canonical.starts_with(PRIVILEGED_PREFIX) {
        return Err(format!(
            "{name} root {} resolves under {PRIVILEGED_PREFIX} — denylisted",
            path.display()
        ));
    }
    Ok(canonical)
}

/// Same invariant for a root that may not exist yet (the Hearth config dir
/// on a first run): the deepest EXISTING ancestor must itself satisfy
/// [`canonical_allowed_root`], and the stored authority is that validated
/// canonical ancestor joined with the literal remainder — no later
/// authority-expanding canonicalization happens.
fn canonical_allowed_root_allowing_missing(name: &str, path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(format!(
            "{name} root must be a non-empty absolute path, got {}",
            path.display()
        ));
    }
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        probe = probe
            .parent()
            .ok_or_else(|| format!("{name} root {} has no existing ancestor", path.display()))?
            .to_path_buf();
    }
    let canonical_ancestor = canonical_allowed_root(name, &probe)?;
    let remainder = path
        .strip_prefix(&probe)
        .map_err(|_| format!("{name} root {} escapes its ancestor", path.display()))?;
    Ok(canonical_ancestor.join(remainder))
}

/// A directory that passed [`verify_user_channel`]. Callers re-check dev/inode
/// identity around temp-file creation + rename (fallback for dirfd binding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDir {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

/// Why a directory failed channel verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRejection {
    pub reason: String,
}

impl std::fmt::Display for ChannelRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for ChannelRejection {}

fn reject(reason: String) -> ChannelRejection {
    ChannelRejection { reason }
}

/// Enumerate all provider binaries per SAPI. A same-version binary from one
/// provider never hides another provider's.
pub fn discover_provider_targets(roots: &ProviderRoots) -> Vec<PhpTargetIdentity> {
    let mut ids = Vec::new();
    for version in crate::php::SUPPORTED_VERSIONS {
        let xy = version.replace('.', "");
        let candidates: [(PhpProvider, PhpSapi, PathBuf); 6] = [
            (
                PhpProvider::Hearth,
                PhpSapi::Cli,
                roots.hearth.join("php").join(version).join("php"),
            ),
            (
                PhpProvider::Hearth,
                PhpSapi::Fpm,
                roots.hearth.join("php").join(version).join("php-fpm"),
            ),
            (
                PhpProvider::Herd,
                PhpSapi::Cli,
                roots.herd.join("bin").join(format!("php{xy}")),
            ),
            (
                PhpProvider::Herd,
                PhpSapi::Fpm,
                roots.herd.join("bin").join(format!("php{xy}-fpm")),
            ),
            (
                PhpProvider::Homebrew,
                PhpSapi::Cli,
                roots
                    .homebrew
                    .join("opt")
                    .join(format!("php@{version}"))
                    .join("bin/php"),
            ),
            (
                PhpProvider::Homebrew,
                PhpSapi::Fpm,
                roots
                    .homebrew
                    .join("opt")
                    .join(format!("php@{version}"))
                    .join("sbin/php-fpm"),
            ),
        ];
        for (provider, sapi, path) in candidates {
            if !path.is_file() {
                continue;
            }
            let binary = path.canonicalize().unwrap_or(path);
            ids.push(PhpTargetIdentity {
                provider,
                version: version.to_string(),
                sapi,
                binary,
            });
        }
    }
    ids
}

/// Probe one binary's scan dirs in both env contexts plus the env-honor
/// canary. CLI probes use `--ini`; FPM probes use `-i` (php-fpm rejects
/// `--ini` with exit 64).
pub async fn probe_scan_dirs(binary: &Path, sapi: PhpSapi, timeout: Duration) -> ProbeInfo {
    let arg = match sapi {
        PhpSapi::Cli => "--ini",
        PhpSapi::Fpm => "-i",
    };
    let mut info = ProbeInfo::default();

    // Normal context: daemon env minus scan-dir overrides (a polluted daemon
    // env must not skew classification).
    let mut normal = tokio::process::Command::new(binary);
    normal
        .arg(arg)
        .env_remove("PHP_INI_SCAN_DIR")
        .env_remove("PHPRC");
    if std::env::var_os("HOME").is_none() {
        info.failures
            .push("normal probe: HOME missing from daemon environment".to_string());
    }
    match run_probe(normal, timeout).await {
        Ok((_status, stdout)) => info.normal_scan_dir = parse_scan_dir(&stdout),
        Err(e) => info.failures.push(format!("normal probe: {e}")),
    }

    // Sanitized context: cleared env, minimal PATH.
    let mut sanitized = tokio::process::Command::new(binary);
    sanitized.arg(arg).env_clear().env("PATH", "/usr/bin:/bin");
    match run_probe(sanitized, timeout).await {
        Ok((_status, stdout)) => info.sanitized_scan_dir = parse_scan_dir(&stdout),
        Err(e) => info.failures.push(format!("sanitized probe: {e}")),
    }

    // Env-honor canary (normal env): scan behavior is launcher-env-dependent
    // (Herd-patched binaries ignore PHP_INI_SCAN_DIR when their
    // HERD_PHP_XY_INI_SCAN_DIR variable is present); env credit requires
    // FUNCTIONAL proof — the marker ini must appear in `Additional .ini
    // files parsed`, never just an echoed scan-dir path.
    match probe_env_honor(binary, arg, timeout).await {
        Ok(true) => info.env_honored = true,
        Ok(false) => {
            info.env_honored = false;
            info.failures.push(
                "canary: marker ini absent from 'Additional .ini files parsed' — \
                 binary does not honor PHP_INI_SCAN_DIR in this context (no env credit)"
                    .to_string(),
            );
        }
        Err(e) => {
            info.env_honored = false;
            info.failures.push(format!("canary probe: {e}"));
        }
    }

    info
}

async fn probe_env_honor(binary: &Path, arg: &str, timeout: Duration) -> Result<bool, String> {
    let canary_dir = std::env::temp_dir().join(format!(
        "hearth-canary-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&canary_dir).map_err(|e| e.to_string())?;
    let marker = canary_dir.join("hearth-canary.ini");
    std::fs::write(&marker, "; hearth env-honor canary\n").map_err(|e| e.to_string())?;

    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg(arg)
        .env_remove("PHPRC")
        .env("PHP_INI_SCAN_DIR", format!(":{}", canary_dir.display()));
    let result = run_probe(cmd, timeout).await;
    let _ = std::fs::remove_dir_all(&canary_dir);

    let (status, stdout) = result?;
    if !status.success() {
        return Err(format!("canary probe exited nonzero ({status})"));
    }
    Ok(parse_parsed_ini_files(&stdout).iter().any(|p| p == &marker))
}

/// Parse the `Additional .ini files parsed` list from `--ini`/`-i` output.
/// Handles the `:`/`=>` field separators, multi-line wrapping with trailing
/// commas, double-quoted paths (PHP 8.5), and `(none)`.
fn parse_parsed_ini_files(stdout: &str) -> Vec<PathBuf> {
    const FIELD: &str = "Additional .ini files parsed";
    let mut files = Vec::new();
    let mut lines = stdout.lines();
    while let Some(line) = lines.next() {
        let Some(idx) = line.find(FIELD) else {
            continue;
        };
        let first = line[idx + FIELD.len()..]
            .trim_start_matches([':', '=', '>'])
            .trim();
        let mut chunks = vec![first.to_string()];
        // Continuation lines: wrapped list entries are bare or quoted
        // absolute paths, one per line, with trailing commas.
        for cont in lines.by_ref() {
            let trimmed = cont.trim();
            if trimmed.is_empty() || !(trimmed.starts_with('/') || trimmed.starts_with('"')) {
                break;
            }
            chunks.push(trimmed.to_string());
        }
        for chunk in chunks {
            for part in chunk.split(',') {
                let path = part.trim().trim_matches('"').trim();
                if path.is_empty() || path == "(none)" {
                    continue;
                }
                files.push(PathBuf::from(path));
            }
        }
        break;
    }
    files
}

async fn run_probe(
    mut cmd: tokio::process::Command,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String), String> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("timed out after {timeout:?}"))?
        .map_err(|e| format!("wait failed: {e}"))?;
    Ok((
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

/// Parse the scan-dir line from `--ini` (CLI) or `-i` (FPM) output.
/// Handles PHP 8.5's double-quoted paths and the raw leading-colon echo seen
/// when `PHP_INI_SCAN_DIR` is set. `(none)`/absent → `None`.
fn parse_scan_dir(stdout: &str) -> Option<PathBuf> {
    const MARKERS: [&str; 2] = [
        "Scan for additional .ini files in:",
        "Scan this dir for additional .ini files =>",
    ];
    for line in stdout.lines() {
        for marker in MARKERS {
            if let Some(idx) = line.find(marker) {
                let mut value = line[idx + marker.len()..].trim();
                value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                let value = value.strip_prefix(':').unwrap_or(value);
                if value.is_empty() || value == "(none)" {
                    return None;
                }
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

/// Verify a probed scan dir as a safe user-owned write channel:
/// canonical-path allowlist, no symlink components, `uid == geteuid()`,
/// no group/other write. Ambient dirs must already exist — this function
/// never creates anything.
pub fn verify_user_channel(
    dir: &Path,
    allowed_roots: &[PathBuf],
) -> Result<VerifiedDir, ChannelRejection> {
    use std::os::unix::fs::MetadataExt;

    // Hard denylist first — applies to the raw path before any fs access.
    if dir.starts_with(PRIVILEGED_PREFIX) {
        return Err(reject(format!(
            "{} is under the shared root-owned {PRIVILEGED_PREFIX} — never written by Hearth",
            dir.display()
        )));
    }

    let leaf_meta = std::fs::symlink_metadata(dir)
        .map_err(|e| reject(format!("{} is not accessible: {e}", dir.display())))?;
    if leaf_meta.file_type().is_symlink() {
        return Err(reject(format!(
            "{} is a symlink — channel dirs must be real directories",
            dir.display()
        )));
    }

    let canonical = dir
        .canonicalize()
        .map_err(|e| reject(format!("cannot canonicalize {}: {e}", dir.display())))?;
    // Re-apply the denylist to the canonical path (a symlink may point in).
    if canonical.starts_with(PRIVILEGED_PREFIX) {
        return Err(reject(format!(
            "{} resolves into {PRIVILEGED_PREFIX} — never written by Hearth",
            dir.display()
        )));
    }
    if canonical != dir {
        return Err(reject(format!(
            "{} contains a symlinked path component (resolves to {})",
            dir.display(),
            canonical.display()
        )));
    }
    if !allowed_roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(reject(format!(
            "{} is outside the allowed channel roots",
            canonical.display()
        )));
    }

    let meta = std::fs::metadata(&canonical)
        .map_err(|e| reject(format!("{}: {e}", canonical.display())))?;
    if !meta.is_dir() {
        return Err(reject(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(reject(format!(
            "{} is not owned by the current user (uid {}) — fix with chown",
            canonical.display(),
            meta.uid()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(reject(format!(
            "{} is group/other-writable (mode {:o}) — fix with chmod go-w",
            canonical.display(),
            meta.mode() & 0o7777
        )));
    }

    Ok(VerifiedDir {
        path: canonical,
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// Create a missing Hearth-owned channel directory through the same trust
/// rules as [`verify_user_channel`] — never called for ambient (Herd or
/// Homebrew) channels, which must preexist.
///
/// Before any filesystem mutation: the target must be lexically under the
/// Hearth root; the deepest EXISTING ancestor is walked component-by-component
/// (each rejected if a symlink), canonicalized, re-checked against the Hearth
/// root and the privileged denylist, and required to be a user-owned,
/// non-group/other-writable directory. Only then are the missing components
/// created (0755), and the final directory must still pass
/// [`verify_user_channel`]. Every failure is returned as a typed rejection —
/// callers surface it as a hard outcome, never a silent skip.
pub fn ensure_hearth_channel_dir(
    hearth_root: &Path,
    dir: &Path,
) -> Result<VerifiedDir, ChannelRejection> {
    use std::os::unix::fs::MetadataExt;

    if !dir.starts_with(hearth_root) {
        return Err(reject(format!(
            "{} is outside the Hearth root {} — only Hearth-owned channels may be created",
            dir.display(),
            hearth_root.display()
        )));
    }
    if dir.starts_with(PRIVILEGED_PREFIX) {
        return Err(reject(format!(
            "{} is under {PRIVILEGED_PREFIX} — never written by Hearth",
            dir.display()
        )));
    }

    // Deepest existing ancestor, with every component from the Hearth root
    // down checked against symlinks BEFORE any mutation.
    let mut ancestor = dir.to_path_buf();
    while !ancestor.exists() {
        match ancestor.parent() {
            Some(parent) => ancestor = parent.to_path_buf(),
            None => {
                return Err(reject(format!(
                    "{} has no existing ancestor",
                    dir.display()
                )));
            }
        }
    }
    let mut walk = hearth_root.to_path_buf();
    let relative = ancestor.strip_prefix(hearth_root).map_err(|_| {
        reject(format!(
            "existing ancestor {} escapes the Hearth root {}",
            ancestor.display(),
            hearth_root.display()
        ))
    })?;
    for component in relative.components() {
        walk.push(component);
        let meta = std::fs::symlink_metadata(&walk)
            .map_err(|e| reject(format!("{}: {e}", walk.display())))?;
        if meta.file_type().is_symlink() {
            return Err(reject(format!(
                "{} is a symlink — refusing to create a channel dir beneath it",
                walk.display()
            )));
        }
    }

    let canonical_ancestor = ancestor
        .canonicalize()
        .map_err(|e| reject(format!("cannot canonicalize {}: {e}", ancestor.display())))?;
    let canonical_root = hearth_root.canonicalize().map_err(|e| {
        reject(format!(
            "cannot canonicalize {}: {e}",
            hearth_root.display()
        ))
    })?;
    if !canonical_ancestor.starts_with(&canonical_root) {
        return Err(reject(format!(
            "existing ancestor {} resolves outside the Hearth root {}",
            canonical_ancestor.display(),
            canonical_root.display()
        )));
    }
    if canonical_ancestor.starts_with(PRIVILEGED_PREFIX) {
        return Err(reject(format!(
            "existing ancestor {} resolves into {PRIVILEGED_PREFIX}",
            canonical_ancestor.display()
        )));
    }
    let meta = std::fs::metadata(&canonical_ancestor)
        .map_err(|e| reject(format!("{}: {e}", canonical_ancestor.display())))?;
    if !meta.is_dir() {
        return Err(reject(format!(
            "{} is not a directory",
            canonical_ancestor.display()
        )));
    }
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(reject(format!(
            "{} is not owned by the current user — refusing to create beneath it",
            canonical_ancestor.display()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(reject(format!(
            "{} is group/other-writable (mode {:o}) — refusing to create beneath it",
            canonical_ancestor.display(),
            meta.mode() & 0o7777
        )));
    }

    std::fs::create_dir_all(dir)
        .map_err(|e| reject(format!("creating {} failed: {e}", dir.display())))?;

    verify_user_channel(dir, std::slice::from_ref(&canonical_root))
}

/// Expected user-channel layout for a (provider, version) pair.
pub fn expected_channel_dir(
    provider: PhpProvider,
    version: &str,
    roots: &ProviderRoots,
) -> PathBuf {
    match provider {
        PhpProvider::Hearth => roots.hearth.join("php").join(version).join("conf.d"),
        PhpProvider::Herd => roots.herd.join("config/php").join(version.replace('.', "")),
        PhpProvider::Homebrew => roots.homebrew.join("etc/php").join(version).join("conf.d"),
    }
}

/// Classify one probed scan dir (one context) for write-side handling.
pub fn classify_channel(
    provider: PhpProvider,
    version: &str,
    probed: Option<&Path>,
    probe_failure: Option<&str>,
    roots: &ProviderRoots,
) -> ChannelClass {
    let Some(probed) = probed else {
        return ChannelClass::BestEffort {
            reason: probe_failure
                .map(str::to_string)
                .unwrap_or_else(|| "binary reports no additional-ini scan directory".to_string()),
        };
    };

    if probed.starts_with(PRIVILEGED_PREFIX) {
        return ChannelClass::PrivilegedDir {
            dir: probed.to_path_buf(),
        };
    }

    let expected = expected_channel_dir(provider, version, roots);
    if probed != expected {
        return ChannelClass::BestEffort {
            reason: format!(
                "scan dir {} does not match the expected {:?} layout {}",
                probed.display(),
                provider,
                expected.display()
            ),
        };
    }

    match verify_user_channel(probed, &roots.allowed_prefixes()) {
        Ok(verified) => ChannelClass::Verified { dir: verified.path },
        Err(rejection) => ChannelClass::BestEffort {
            reason: format!("channel verification failed: {rejection}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);

    /// Canonicalized tempdir (macOS /var/folders is itself a symlink into
    /// /private/var — canonicalize once so channel verification sees stable paths).
    fn canon_tmp() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = tmp.path().canonicalize().unwrap();
        (tmp, canonical)
    }

    fn write_fake_php(dir: &Path, name: &str, script: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        // macOS stalls the FIRST exec of a fresh unsigned executable for
        // seconds while syspolicyd assesses it. Absorb that one-time cost
        // here (unbounded) so probe timeouts measure the probe itself.
        let _ = std::process::Command::new(&path)
            .arg("--warmup")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        path
    }

    /// Stock-build fake: honors PHP_INI_SCAN_DIR in every context, reports a
    /// different compiled-in scan dir with and without HOME.
    fn stock_fake(normal_dir: &Path, sanitized_dir: &Path) -> String {
        format!(
            r#"#!/bin/sh
if [ -n "$PHP_INI_SCAN_DIR" ]; then
    echo "Scan for additional .ini files in: $PHP_INI_SCAN_DIR"
    parsed="${{PHP_INI_SCAN_DIR#:}}"
    echo "Additional .ini files parsed:      $parsed/hearth-canary.ini"
    exit 0
fi
if [ -n "$HOME" ]; then
    echo "Scan for additional .ini files in: {normal}"
else
    echo "Scan for additional .ini files in: {sanitized}"
fi
"#,
            normal = normal_dir.display(),
            sanitized = sanitized_dir.display(),
        )
    }

    /// Launcher-env-patched fake, modeled on real Herd binaries: when its
    /// vendor scan-dir variable is present in the environment (simulated
    /// here via `$HOME` as an always-present ambient variable in the normal
    /// probe context) it IGNORES `PHP_INI_SCAN_DIR` entirely; only a cleared
    /// env honors the variable. Real trigger on the reference machine is
    /// `HERD_PHP_XY_INI_SCAN_DIR`, not HOME — the mechanism under test
    /// (canary must catch an env-ignoring binary) is identical.
    fn herd_fake(normal_dir: &Path, sanitized_dir: &Path) -> String {
        format!(
            r#"#!/bin/sh
if [ -n "$HOME" ]; then
    echo "Scan for additional .ini files in: {normal}"
    exit 0
fi
if [ -n "$PHP_INI_SCAN_DIR" ]; then
    echo "Scan for additional .ini files in: $PHP_INI_SCAN_DIR"
    parsed="${{PHP_INI_SCAN_DIR#:}}"
    echo "Additional .ini files parsed:      $parsed/hearth-canary.ini"
else
    echo "Scan this dir for additional .ini files => {sanitized}"
fi
"#,
            normal = normal_dir.display(),
            sanitized = sanitized_dir.display(),
        )
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

    // ---- probing ----

    #[tokio::test]
    async fn probe_parses_cli_scan_dir_line() {
        let (_tmp, base) = canon_tmp();
        let bin_dir = base.join("php bin dir with spaces");
        let normal = base.join("normal conf.d");
        let sanitized = base.join("sanitized conf.d");
        let php = write_fake_php(&bin_dir, "php", &stock_fake(&normal, &sanitized));

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert_eq!(probe.normal_scan_dir.as_deref(), Some(normal.as_path()));
    }

    #[tokio::test]
    async fn probe_fpm_uses_dash_i() {
        let (_tmp, base) = canon_tmp();
        let dir = base.join("fpm conf.d");
        // Rejects anything but `-i` with exit 64, mirroring real php-fpm's
        // rejection of `--ini`.
        let script = format!(
            r#"#!/bin/sh
if [ "$1" != "-i" ]; then
    echo "invalid argument" >&2
    exit 64
fi
echo "Scan this dir for additional .ini files => {dir}"
"#,
            dir = dir.display()
        );
        let fpm = write_fake_php(&base.join("sbin dir"), "php-fpm", &script);

        let probe = probe_scan_dirs(&fpm, PhpSapi::Fpm, PROBE_TIMEOUT).await;
        assert_eq!(probe.normal_scan_dir.as_deref(), Some(dir.as_path()));
    }

    #[tokio::test]
    async fn probe_strips_quoted_paths() {
        // PHP 8.5 prints scan-dir paths double-quoted.
        let (_tmp, base) = canon_tmp();
        let dir = base.join("quoted conf.d");
        let script = format!(
            "#!/bin/sh\necho 'Scan for additional .ini files in: \"{}\"'\n",
            dir.display()
        );
        let php = write_fake_php(&base.join("bin"), "php", &script);

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert_eq!(probe.normal_scan_dir.as_deref(), Some(dir.as_path()));
    }

    #[tokio::test]
    async fn probe_tolerates_leading_colon_echo() {
        // With PHP_INI_SCAN_DIR set, the line echoes the raw value including
        // the leading colon.
        let (_tmp, base) = canon_tmp();
        let dir = base.join("echo conf.d");
        let script = format!(
            "#!/bin/sh\necho 'Scan for additional .ini files in: :{}'\n",
            dir.display()
        );
        let php = write_fake_php(&base.join("bin"), "php", &script);

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert_eq!(probe.normal_scan_dir.as_deref(), Some(dir.as_path()));
    }

    #[tokio::test]
    async fn probe_none_when_absent() {
        let (_tmp, base) = canon_tmp();
        let script = "#!/bin/sh\necho 'Scan for additional .ini files in: (none)'\n";
        let php = write_fake_php(&base.join("bin"), "php", script);

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert_eq!(probe.normal_scan_dir, None);
    }

    #[tokio::test]
    async fn probe_times_out_returns_besteffort() {
        let (_tmp, base) = canon_tmp();
        let php = write_fake_php(&base.join("bin"), "php", "#!/bin/sh\nsleep 5\n");
        let roots = test_roots(&base);

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, SHORT_TIMEOUT).await;
        assert_eq!(probe.normal_scan_dir, None);
        assert!(
            !probe.failures.is_empty(),
            "timeout must be recorded as a probe failure"
        );

        let class = classify_channel(
            PhpProvider::Herd,
            "8.4",
            probe.normal_scan_dir.as_deref(),
            probe.failures.first().map(String::as_str),
            &roots,
        );
        match class {
            ChannelClass::BestEffort { reason } => {
                assert!(
                    reason.to_lowercase().contains("time"),
                    "BestEffort reason should mention the timeout: {reason}"
                );
            }
            other => panic!("expected BestEffort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sanitized_probe_clears_home() {
        let (_tmp, base) = canon_tmp();
        let normal = base.join("normal conf.d");
        let sanitized = base.join("sanitized conf.d");
        let php = write_fake_php(&base.join("bin"), "php", &stock_fake(&normal, &sanitized));

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert_eq!(probe.normal_scan_dir.as_deref(), Some(normal.as_path()));
        assert_eq!(
            probe.sanitized_scan_dir.as_deref(),
            Some(sanitized.as_path()),
            "sanitized probe must clear HOME so the fake takes its no-HOME branch"
        );
    }

    #[tokio::test]
    async fn env_honor_canary_detects_ignoring_binary() {
        let (_tmp, base) = canon_tmp();
        let normal = base.join("normal conf.d");
        let sanitized = base.join("sanitized conf.d");

        let herd = write_fake_php(
            &base.join("herd bin"),
            "php85",
            &herd_fake(&normal, &sanitized),
        );
        let probe = probe_scan_dirs(&herd, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert!(
            !probe.env_honored,
            "Herd-patched fake drops PHP_INI_SCAN_DIR when HOME is set — no env credit"
        );

        let stock = write_fake_php(
            &base.join("brew bin"),
            "php",
            &stock_fake(&normal, &sanitized),
        );
        let probe = probe_scan_dirs(&stock, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert!(probe.env_honored, "stock fake honors the env var");
    }

    #[tokio::test]
    async fn env_honor_canary_rejects_echo_only_binary() {
        // Echoes the requested scan dir back but reports that NO additional
        // ini files were parsed — functional loading did not happen, so no
        // env credit may be granted.
        let (_tmp, base) = canon_tmp();
        let normal = base.join("normal conf.d");
        let script = format!(
            r#"#!/bin/sh
if [ -n "$PHP_INI_SCAN_DIR" ]; then
    echo "Scan for additional .ini files in: $PHP_INI_SCAN_DIR"
    echo "Additional .ini files parsed:      (none)"
else
    echo "Scan for additional .ini files in: {normal}"
fi
"#,
            normal = normal.display()
        );
        let php = write_fake_php(&base.join("bin"), "php", &script);

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert!(
            !probe.env_honored,
            "an echoed scan dir without the marker in the parsed list is not proof"
        );
        assert!(
            probe.failures.iter().any(|f| f.contains("parsed")),
            "degradation reason must explain the missing parsed marker: {:?}",
            probe.failures
        );
    }

    #[tokio::test]
    async fn env_honor_canary_accepts_marker_in_wrapped_quoted_list() {
        // Realistic `--ini` output wraps the parsed list across lines with
        // trailing commas; PHP 8.5 quotes paths. The exact marker path in
        // that list must earn credit.
        let (_tmp, base) = canon_tmp();
        let normal = base.join("normal conf.d");
        let script = format!(
            r#"#!/bin/sh
if [ -n "$PHP_INI_SCAN_DIR" ]; then
    parsed="${{PHP_INI_SCAN_DIR#:}}"
    echo "Scan for additional .ini files in: $PHP_INI_SCAN_DIR"
    echo "Additional .ini files parsed:      /opt/fake/conf.d/10-first.ini,"
    echo "\"$parsed/hearth-canary.ini\","
    echo "/opt/fake/conf.d/zz-last.ini"
else
    echo "Scan for additional .ini files in: {normal}"
fi
"#,
            normal = normal.display()
        );
        let php = write_fake_php(&base.join("bin"), "php", &script);

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert!(
            probe.env_honored,
            "exact marker path in the wrapped/quoted parsed list must earn credit: {:?}",
            probe.failures
        );
    }

    #[tokio::test]
    async fn env_honor_canary_rejects_nonzero_exit() {
        // Output looks perfect but the probe exits nonzero — no credit.
        let (_tmp, base) = canon_tmp();
        let normal = base.join("normal conf.d");
        let script = format!(
            r#"#!/bin/sh
if [ -n "$PHP_INI_SCAN_DIR" ]; then
    parsed="${{PHP_INI_SCAN_DIR#:}}"
    echo "Scan for additional .ini files in: $PHP_INI_SCAN_DIR"
    echo "Additional .ini files parsed:      $parsed/hearth-canary.ini"
    exit 3
fi
echo "Scan for additional .ini files in: {normal}"
"#,
            normal = normal.display()
        );
        let php = write_fake_php(&base.join("bin"), "php", &script);

        let probe = probe_scan_dirs(&php, PhpSapi::Cli, PROBE_TIMEOUT).await;
        assert!(
            !probe.env_honored,
            "nonzero canary exit must not earn credit"
        );
        assert!(
            probe.failures.iter().any(|f| f.contains("exit")),
            "failure must mention the nonzero exit: {:?}",
            probe.failures
        );
    }

    // ---- discovery ----

    #[tokio::test]
    async fn discovery_emits_both_providers_for_same_version() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        write_fake_php(&roots.herd.join("bin"), "php85", "#!/bin/sh\n");
        write_fake_php(&roots.herd.join("bin"), "php85-fpm", "#!/bin/sh\n");
        write_fake_php(
            &roots.homebrew.join("opt/php@8.5/bin"),
            "php",
            "#!/bin/sh\n",
        );
        write_fake_php(
            &roots.homebrew.join("opt/php@8.5/sbin"),
            "php-fpm",
            "#!/bin/sh\n",
        );

        let ids = discover_provider_targets(&roots);
        assert_eq!(ids.len(), 4, "no cross-provider dedup: {ids:?}");
        assert!(ids.iter().all(|id| id.version == "8.5"));
        for provider in [PhpProvider::Herd, PhpProvider::Homebrew] {
            for sapi in [PhpSapi::Cli, PhpSapi::Fpm] {
                assert!(
                    ids.iter()
                        .any(|id| id.provider == provider && id.sapi == sapi),
                    "missing {provider:?}/{sapi:?} in {ids:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn discover_yields_cli_and_fpm_per_provider() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let cli = write_fake_php(&roots.hearth.join("php/8.4"), "php", "#!/bin/sh\n");
        let fpm = write_fake_php(&roots.hearth.join("php/8.4"), "php-fpm", "#!/bin/sh\n");

        let ids = discover_provider_targets(&roots);
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| id.provider == PhpProvider::Hearth));
        assert!(
            ids.iter()
                .any(|id| id.sapi == PhpSapi::Cli && id.binary == cli)
        );
        assert!(
            ids.iter()
                .any(|id| id.sapi == PhpSapi::Fpm && id.binary == fpm)
        );
    }

    // ---- classification & channel verification ----

    #[test]
    fn classify_homebrew_verified() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let dir = roots.homebrew.join("etc/php/8.5/conf.d");
        std::fs::create_dir_all(&dir).unwrap();

        let class = classify_channel(PhpProvider::Homebrew, "8.5", Some(&dir), None, &roots);
        assert_eq!(class, ChannelClass::Verified { dir });
    }

    #[test]
    fn classify_herd_normal_plus_privileged_sanitized() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let herd_dir = roots.herd.join("config/php/84");
        std::fs::create_dir_all(&herd_dir).unwrap();

        let normal = classify_channel(PhpProvider::Herd, "8.4", Some(&herd_dir), None, &roots);
        assert_eq!(normal, ChannelClass::Verified { dir: herd_dir });

        let privileged = PathBuf::from("/usr/local/etc/php/conf.d");
        let sanitized = classify_channel(PhpProvider::Herd, "8.4", Some(&privileged), None, &roots);
        assert_eq!(sanitized, ChannelClass::PrivilegedDir { dir: privileged });
    }

    #[test]
    fn denylist_rejects_usr_local_prefix() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        // The /usr/local/etc/php prefix is privileged regardless of provider,
        // layout expectations, or probe context.
        for probed in ["/usr/local/etc/php/conf.d", "/usr/local/etc/php/8.4/conf.d"] {
            let class = classify_channel(
                PhpProvider::Homebrew,
                "8.4",
                Some(Path::new(probed)),
                None,
                &roots,
            );
            assert_eq!(
                class,
                ChannelClass::PrivilegedDir {
                    dir: PathBuf::from(probed)
                },
                "{probed} must classify as PrivilegedDir"
            );
        }
    }

    #[test]
    fn verify_rejects_symlink_leaf() {
        let (_tmp, base) = canon_tmp();
        let real = base.join("real-conf.d");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link-conf.d");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = verify_user_channel(&link, std::slice::from_ref(&base)).unwrap_err();
        assert!(
            err.reason.to_lowercase().contains("symlink"),
            "rejection must name the symlink: {}",
            err.reason
        );
    }

    #[test]
    fn verify_rejects_symlinked_ancestor_escaping_allowlist() {
        let (_tmp_a, allowed) = canon_tmp();
        let (_tmp_b, outside) = canon_tmp();
        std::fs::create_dir_all(outside.join("conf.d")).unwrap();
        std::os::unix::fs::symlink(&outside, allowed.join("mid")).unwrap();
        let dir = allowed.join("mid/conf.d");

        assert!(
            verify_user_channel(&dir, std::slice::from_ref(&allowed)).is_err(),
            "canonicalization must expose the ancestor symlink escaping the allowlist"
        );
    }

    #[test]
    fn verify_rejects_group_writable() {
        let (_tmp, base) = canon_tmp();
        let dir = base.join("loose-conf.d");
        std::fs::create_dir_all(&dir).unwrap();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o775);
        std::fs::set_permissions(&dir, perms).unwrap();

        let err = verify_user_channel(&dir, std::slice::from_ref(&base)).unwrap_err();
        assert!(
            err.reason.to_lowercase().contains("writable"),
            "rejection must explain the write-mask problem: {}",
            err.reason
        );
    }

    #[test]
    fn ambient_channel_must_preexist() {
        let (_tmp, base) = canon_tmp();
        let roots = test_roots(&base);
        let expected = roots.homebrew.join("etc/php/8.1/conf.d");
        assert!(!expected.exists());

        let class = classify_channel(PhpProvider::Homebrew, "8.1", Some(&expected), None, &roots);
        assert!(
            matches!(class, ChannelClass::BestEffort { .. }),
            "missing ambient channel is BestEffort, got {class:?}"
        );
        assert!(
            !expected.exists(),
            "classification must never create an ambient (non-Hearth) channel dir"
        );
    }

    #[test]
    fn ensure_hearth_channel_dir_creates_and_verifies() {
        let (_tmp, base) = canon_tmp();
        let hearth_root = base.join("hearth");
        std::fs::create_dir_all(&hearth_root).unwrap();
        let dir = hearth_root.join("php/8.4/conf.d");

        let verified = ensure_hearth_channel_dir(&hearth_root, &dir).unwrap();
        assert_eq!(verified.path, dir);
        assert!(dir.is_dir());
        // Idempotent on an existing dir.
        assert!(ensure_hearth_channel_dir(&hearth_root, &dir).is_ok());
    }

    #[test]
    fn ensure_hearth_channel_dir_rejects_outside_root() {
        let (_tmp, base) = canon_tmp();
        let hearth_root = base.join("hearth");
        std::fs::create_dir_all(&hearth_root).unwrap();
        let err =
            ensure_hearth_channel_dir(&hearth_root, &base.join("elsewhere/conf.d")).unwrap_err();
        assert!(err.reason.contains("outside the Hearth root"), "got: {err}");
    }

    #[test]
    fn ensure_hearth_channel_dir_rejects_symlinked_ancestor() {
        let (_tmp, base) = canon_tmp();
        let hearth_root = base.join("hearth");
        std::fs::create_dir_all(&hearth_root).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, hearth_root.join("php")).unwrap();

        let err = ensure_hearth_channel_dir(&hearth_root, &hearth_root.join("php/8.4/conf.d"))
            .unwrap_err();
        assert!(err.reason.contains("symlink"), "got: {err}");
        assert!(
            !outside.join("8.4").exists(),
            "zero mutation through the symlink"
        );
    }

    #[test]
    fn lone_provider_override_without_isolated_root_is_rejected() {
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
            "HEARTH_CONFIG_DIR",
        ]);
        guard.remove("HEARTH_ISOLATED_ROOT");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.remove("HEARTH_CONFIG_DIR");
        guard.set("HEARTH_HERD_ROOT", "/tmp/evil-discovery-root");
        let err = ProviderRoots::detect().unwrap_err();
        assert!(err.contains("HEARTH_ISOLATED_ROOT"), "got: {err}");
        drop(guard);
    }

    #[test]
    fn present_but_empty_provider_override_fails_closed() {
        // B3-2: an explicitly present empty override is a configuration
        // ERROR, never silently "unset".
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
            "HEARTH_CONFIG_DIR",
        ]);
        guard.remove("HEARTH_ISOLATED_ROOT");
        guard.remove("HEARTH_CONFIG_DIR");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.set("HEARTH_HERD_ROOT", "");
        let err = ProviderRoots::detect().unwrap_err();
        assert!(err.contains("empty"), "got: {err}");

        // Same discipline for the homebrew override…
        guard.remove("HEARTH_HERD_ROOT");
        guard.set("HEARTH_HOMEBREW_ROOT", "");
        let err = ProviderRoots::detect().unwrap_err();
        assert!(err.contains("empty"), "got: {err}");

        // …and in isolated mode: a present empty provider root never
        // silently defaults beneath the runtime root.
        let (_tmp, base) = canon_tmp();
        std::fs::create_dir_all(base.join("hearth")).unwrap();
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.set("HEARTH_ISOLATED_ROOT", &base);
        guard.set("HEARTH_CONFIG_DIR", base.join("hearth"));
        guard.set("HEARTH_HERD_ROOT", "");
        let err = ProviderRoots::detect().unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
        drop(guard);
    }

    #[test]
    fn production_write_authority_is_immutable_and_never_tmp() {
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
            "HEARTH_CONFIG_DIR",
        ]);
        guard.remove("HEARTH_ISOLATED_ROOT");
        guard.remove("HEARTH_HERD_ROOT");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.remove("HEARTH_CONFIG_DIR");
        let roots = ProviderRoots::detect().unwrap();
        let allowed = roots.allowed_prefixes();
        drop(guard);

        assert_eq!(
            allowed.len(),
            3,
            "config dir + home + /opt/homebrew: {allowed:?}"
        );
        assert!(
            allowed
                .iter()
                .all(|p| !p.starts_with("/tmp") && !p.starts_with("/private/tmp")),
            "no temp path may ever be production write authority: {allowed:?}"
        );
        assert!(
            allowed.iter().any(|p| p == Path::new("/opt/homebrew")),
            "got: {allowed:?}"
        );
    }

    #[test]
    fn home_unavailable_or_root_fails_closed_never_slash_allowlist() {
        // B3-2: no fallback-to-`/` ever; HOME-unavailable is a typed error.
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
            "HEARTH_CONFIG_DIR",
        ]);
        guard.remove("HEARTH_ISOLATED_ROOT");
        guard.remove("HEARTH_HERD_ROOT");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.remove("HEARTH_CONFIG_DIR");

        let err = ProviderRoots::detect_with_home(None).unwrap_err();
        assert!(err.contains("home"), "got: {err}");
        assert!(err.contains("fail closed"), "got: {err}");

        let err = ProviderRoots::detect_with_home(Some(PathBuf::from("/"))).unwrap_err();
        assert!(err.contains("can never be write authority"), "got: {err}");

        // A valid home yields an allowlist that never contains `/`.
        let (_tmp, base) = canon_tmp();
        let roots = ProviderRoots::detect_with_home(Some(base.clone())).unwrap();
        let allowed = roots.allowed_prefixes();
        assert!(
            allowed.iter().all(|p| p != Path::new("/")),
            "`/` must never be write authority: {allowed:?}"
        );
        assert!(allowed.contains(&base));
        drop(guard);
    }

    #[test]
    fn home_symlink_alias_to_root_or_denylist_is_rejected() {
        // B4-2: canonicalization happens FIRST — an absolute HOME that is a
        // symlink alias of `/` (or a denylisted root) must be rejected and
        // can never appear in allowed prefixes.
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
            "HEARTH_CONFIG_DIR",
        ]);
        guard.remove("HEARTH_ISOLATED_ROOT");
        guard.remove("HEARTH_HERD_ROOT");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.remove("HEARTH_CONFIG_DIR");
        let (_tmp, base) = canon_tmp();
        let root_alias = base.join("home-alias-root");
        std::os::unix::fs::symlink("/", &root_alias).unwrap();
        let err = ProviderRoots::detect_with_home(Some(root_alias)).unwrap_err();
        assert!(err.contains("/"), "got: {err}");

        let deny_alias = base.join("home-alias-denylist");
        std::os::unix::fs::symlink("/usr/local/etc/php", &deny_alias).unwrap();
        assert!(ProviderRoots::detect_with_home(Some(deny_alias)).is_err());

        // Missing / non-directory HOME fails closed too.
        assert!(ProviderRoots::detect_with_home(Some(base.join("no-such-home"))).is_err());
        let file_home = base.join("file-home");
        std::fs::write(&file_home, "x").unwrap();
        assert!(ProviderRoots::detect_with_home(Some(file_home)).is_err());
        drop(guard);
    }

    #[test]
    fn isolated_runtime_root_slash_or_alias_is_rejected() {
        // B4-2: even the test-only constructor keeps the invariant — a
        // runtime root that IS or resolves to `/` (or a denylisted root) is
        // rejected outright.
        let err = ProviderRoots::isolated(
            Path::new("/"),
            PathBuf::from("/hearth"),
            PathBuf::from("/herd"),
            PathBuf::from("/homebrew"),
        )
        .unwrap_err();
        assert!(err.contains("/"), "got: {err}");

        let (_tmp, base) = canon_tmp();
        let alias = base.join("runtime-alias-root");
        std::os::unix::fs::symlink("/", &alias).unwrap();
        assert!(
            ProviderRoots::isolated(
                &alias,
                alias.join("hearth"),
                alias.join("herd"),
                alias.join("homebrew"),
            )
            .is_err(),
            "runtime root aliasing `/` must be rejected"
        );
    }

    #[test]
    fn allowed_prefixes_can_never_contain_slash() {
        // Post-invariant: stored prefixes are pre-validated canonical form;
        // no constructor path may yield `/`.
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
            "HEARTH_CONFIG_DIR",
        ]);
        guard.remove("HEARTH_ISOLATED_ROOT");
        guard.remove("HEARTH_HERD_ROOT");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.remove("HEARTH_CONFIG_DIR");
        let (_tmp, base) = canon_tmp();
        std::fs::create_dir_all(base.join("hearth")).unwrap();
        let production = ProviderRoots::detect_with_home(Some(base.clone())).unwrap();
        let isolated = ProviderRoots::isolated(
            &base,
            base.join("hearth"),
            base.join("herd"),
            base.join("homebrew"),
        )
        .unwrap();
        for roots in [production, isolated] {
            assert!(
                roots
                    .allowed_prefixes()
                    .iter()
                    .all(|p| p != Path::new("/") && p.is_absolute()),
                "invariant violated: {:?}",
                roots.allowed_prefixes()
            );
        }
        drop(guard);
    }

    #[test]
    fn isolated_mode_confines_and_validates_roots() {
        let (_tmp, base) = canon_tmp();
        std::fs::create_dir_all(base.join("hearth")).unwrap();

        // Roots outside the runtime root are rejected.
        let err = ProviderRoots::isolated(
            &base,
            base.join("hearth"),
            PathBuf::from("/tmp"),
            base.join("homebrew"),
        )
        .unwrap_err();
        assert!(err.contains("not contained"), "got: {err}");

        // Relative/empty roots are rejected.
        let err = ProviderRoots::isolated(
            &base,
            base.join("hearth"),
            PathBuf::from("relative/herd"),
            base.join("homebrew"),
        )
        .unwrap_err();
        assert!(err.contains("absolute"), "got: {err}");

        // Valid isolated construction confines write authority to the root,
        // and /usr/local stays denied there too.
        let roots = ProviderRoots::isolated(
            &base,
            base.join("hearth"),
            base.join("herd"),
            base.join("homebrew"),
        )
        .unwrap();
        assert_eq!(roots.allowed_prefixes(), vec![base.clone()]);
        assert!(
            verify_user_channel(
                Path::new("/usr/local/etc/php/conf.d"),
                &roots.allowed_prefixes()
            )
            .is_err()
        );
    }

    #[test]
    fn isolated_env_mode_requires_config_dir_under_root() {
        let (_tmp, base) = canon_tmp();
        std::fs::create_dir_all(base.join("hearth")).unwrap();
        let guard = crate::test_env::EnvGuard::capture([
            "HEARTH_ISOLATED_ROOT",
            "HEARTH_HERD_ROOT",
            "HEARTH_HOMEBREW_ROOT",
            "HEARTH_CONFIG_DIR",
        ]);
        guard.remove("HEARTH_HERD_ROOT");
        guard.remove("HEARTH_HOMEBREW_ROOT");
        guard.set("HEARTH_ISOLATED_ROOT", &base);
        guard.set("HEARTH_CONFIG_DIR", base.join("hearth"));

        let roots = ProviderRoots::detect().unwrap();
        assert_eq!(roots.allowed_prefixes(), vec![base.clone()]);
        assert_eq!(roots.herd, base.join("herd"));

        // Real (outside-root) config dir in isolated mode fails closed.
        guard.remove("HEARTH_CONFIG_DIR");
        let err = ProviderRoots::detect().unwrap_err();
        assert!(err.contains("not contained"), "got: {err}");
        drop(guard);
    }

    #[test]
    fn ensure_hearth_channel_dir_rejects_group_writable_ancestor() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, base) = canon_tmp();
        let hearth_root = base.join("hearth");
        let ancestor = hearth_root.join("php");
        std::fs::create_dir_all(&ancestor).unwrap();
        std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o777)).unwrap();

        let err =
            ensure_hearth_channel_dir(&hearth_root, &ancestor.join("8.4/conf.d")).unwrap_err();
        assert!(err.reason.contains("group/other-writable"), "got: {err}");
        assert!(!ancestor.join("8.4").exists());
    }
}
