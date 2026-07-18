//! Provider-explicit PHP target discovery, SAPI-aware scan-dir probing, and
//! write-channel classification.
//!
//! Identity is `(provider, version, sapi, canonical_binary)` — a same-version
//! binary from one provider NEVER hides another provider's (Herd php85 and
//! Homebrew php@8.5 are co-installed realities, not duplicates).
//!
//! Probing runs the real binary (`--ini` for CLI, `-i` for FPM — php-fpm has
//! no `--ini`) in two env contexts plus an env-honor canary, because
//! Herd-patched binaries ignore `PHP_INI_SCAN_DIR` whenever `HOME` is set.

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
    /// Env-honor canary result: does the binary honor `PHP_INI_SCAN_DIR`
    /// under a normal (HOME-bearing) environment? Herd-patched binaries do not.
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

/// A discovered PHP target with probe results and per-context classification.
#[derive(Debug, Clone)]
pub struct PhpTarget {
    pub id: PhpTargetIdentity,
    pub probe: ProbeInfo,
    pub normal_channel: ChannelClass,
    pub sanitized_channel: ChannelClass,
    /// `Some` iff a Verified channel may receive `zz-hearth.ini`.
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
}

impl ProviderRoots {
    /// Production roots.
    pub fn detect() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Self {
            hearth: crate::config_dir(),
            herd: home.join("Library/Application Support/Herd"),
            homebrew: PathBuf::from("/opt/homebrew"),
        }
    }

    /// Canonicalized channel-verification allowlist derived from the roots
    /// (all under `$HOME` or `/opt/homebrew` in production).
    pub fn allowed_prefixes(&self) -> Vec<PathBuf> {
        [&self.hearth, &self.herd, &self.homebrew]
            .into_iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect()
    }
}

/// Shared root-owned, version-blind fallback scan dir — hard-denylisted:
/// never written, never verified, always `PrivilegedDir`.
const PRIVILEGED_PREFIX: &str = "/usr/local/etc/php";

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
        Ok(stdout) => info.normal_scan_dir = parse_scan_dir(&stdout),
        Err(e) => info.failures.push(format!("normal probe: {e}")),
    }

    // Sanitized context: cleared env, minimal PATH.
    let mut sanitized = tokio::process::Command::new(binary);
    sanitized.arg(arg).env_clear().env("PATH", "/usr/bin:/bin");
    match run_probe(sanitized, timeout).await {
        Ok(stdout) => info.sanitized_scan_dir = parse_scan_dir(&stdout),
        Err(e) => info.failures.push(format!("sanitized probe: {e}")),
    }

    // Env-honor canary (normal env): Herd-patched binaries ignore
    // PHP_INI_SCAN_DIR whenever HOME is set; env credit requires proof.
    match probe_env_honor(binary, arg, timeout).await {
        Ok(honored) => info.env_honored = honored,
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

    let stdout = result?;
    Ok(stdout.contains(&canary_dir.display().to_string()))
}

async fn run_probe(mut cmd: tokio::process::Command, timeout: Duration) -> Result<String, String> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("timed out after {timeout:?}"))?
        .map_err(|e| format!("wait failed: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

    /// Herd-patched fake: with HOME set it IGNORES PHP_INI_SCAN_DIR entirely;
    /// only a cleared env honors the variable.
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
        ProviderRoots {
            hearth: base.join("hearth"),
            herd: base.join("Herd"),
            homebrew: base.join("homebrew"),
        }
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
}
