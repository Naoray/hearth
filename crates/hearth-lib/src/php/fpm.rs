use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::reconcile::{
    ArtifactDirVerifier, DirIdentity, EntryState, FileAction, FinalizeMode, Manifest,
    ManifestEntry, OwnedArtifactSpec, WriteOutcome, manifest_set_transaction, sha256_hex,
};
use super::targets::{ensure_hearth_channel_dir, verify_user_channel};

const SUN_PATH_BYTES: usize = 104;

pub const FPM_CONF_HEADER_V1: &str = "; managed by hearth — do not edit. `hearth php config --sync` regenerates this file.\n; hearth-owned: fpm-conf v1\n\n";

pub const PROBE_SCRIPT_V1: &str = r#"<?php
// managed by hearth — do not edit. `hearth php config --sync` regenerates this file.
header('Content-Type: text/plain');
$k = $_SERVER['HEARTH_PROBE_KEY'] ?? '';
$n = $_SERVER['HEARTH_PROBE_NONCE'] ?? '';
$v = ($k !== '') ? ini_get($k) : false;
echo json_encode([
    'hearth_probe' => 1,
    'nonce' => $n,
    'key' => $k,
    'pid' => getmypid(),
    'available' => $v !== false,
    'value' => $v === false ? null : (string) $v,
]);
"#;

pub fn socket_path(config_dir: &Path) -> PathBuf {
    config_dir.join("run/php-fpm.sock")
}

pub fn conf_path(config_dir: &Path) -> PathBuf {
    config_dir.join("fpm/php-fpm.conf")
}

pub fn probe_script_path(config_dir: &Path) -> PathBuf {
    config_dir.join("fpm/hearth-probe.php")
}

pub fn fpm_manifest_path(config_dir: &Path) -> PathBuf {
    config_dir.join("fpm/manifest.toml")
}

fn rendered_path<'a>(label: &str, path: &'a Path) -> Result<&'a str, String> {
    let bytes = path.as_os_str().as_bytes();
    if bytes
        .iter()
        .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0'))
    {
        return Err(format!(
            "{label} path '{}' must not contain CR, LF, or NUL",
            path.display()
        ));
    }
    path.to_str()
        .ok_or_else(|| format!("{label} path is not valid UTF-8: {}", path.display()))
}

pub(crate) fn render_fpm_conf_v(version: u32, config_dir: &Path) -> Result<String, String> {
    rendered_path("config directory", config_dir)?;
    rendered_path("FPM config", &conf_path(config_dir))?;

    let listener_path = socket_path(config_dir);
    let listener = rendered_path("FPM listener", &listener_path)?;
    let listener_bytes = listener_path.as_os_str().as_bytes().len();
    if listener_bytes + 1 > SUN_PATH_BYTES {
        return Err(format!(
            "config dir too deep for a unix listener socket ({} bytes; limit 103 + NUL): {}",
            listener_bytes,
            config_dir.display()
        ));
    }

    let log_path = config_dir.join("log/php-fpm.log");
    let log = rendered_path("FPM error log", &log_path)?;
    let header = if version == 1 {
        FPM_CONF_HEADER_V1.to_string()
    } else {
        format!(
            "; managed by hearth — do not edit. `hearth php config --sync` regenerates this file.\n; hearth-owned: fpm-conf v{version}\n\n"
        )
    };

    Ok(format!(
        "{header}[global]\nerror_log = {log}\ndaemonize = no\n\n[hearth]\nlisten = {listener}\nlisten.mode = 0600\npm = ondemand\npm.max_children = 10\npm.process_idle_timeout = 10s\npm.max_requests = 500\ncatch_workers_output = yes\nclear_env = yes\nsecurity.limit_extensions = .php\n"
    ))
}

pub fn render_fpm_conf(config_dir: &Path) -> Result<String, String> {
    render_fpm_conf_v(1, config_dir)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FpmConfState {
    HearthOwned {
        conf: PathBuf,
        conf_sha256: String,
        probe_sha256: String,
        listen: PathBuf,
    },
    UserManaged {
        conf: PathBuf,
    },
    Blocked {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeReport {
    pub conf: WriteOutcome,
    pub probe: WriteOutcome,
    pub state: FpmConfState,
}

struct HearthFpmDirVerifier<'a> {
    hearth_root: &'a Path,
    dir: &'a Path,
}

impl ArtifactDirVerifier for HearthFpmDirVerifier<'_> {
    fn verify(&self) -> Result<DirIdentity, String> {
        let verified = ensure_hearth_channel_dir(self.hearth_root, self.dir)
            .map_err(|error| error.to_string())?;
        Ok(DirIdentity {
            dev: verified.dev,
            ino: verified.ino,
        })
    }

    fn recheck(&self, identity: &DirIdentity) -> Result<(), String> {
        let metadata = std::fs::metadata(self.dir).map_err(|error| error.to_string())?;
        if metadata.dev() == identity.dev && metadata.ino() == identity.ino {
            Ok(())
        } else {
            Err("FPM directory was replaced during the write".to_string())
        }
    }
}

fn blocked_report(reason: String) -> MaterializeReport {
    MaterializeReport {
        conf: WriteOutcome::Failed {
            error: reason.clone(),
        },
        probe: WriteOutcome::Failed {
            error: reason.clone(),
        },
        state: FpmConfState::Blocked { reason },
    }
}

fn ensure_private_dir(hearth_root: &Path, dir: &Path) -> Result<(), String> {
    ensure_hearth_channel_dir(hearth_root, dir).map_err(|error| error.to_string())?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not secure {}: {error}", dir.display()))?;
    ensure_hearth_channel_dir(hearth_root, dir).map_err(|error| error.to_string())?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, String> {
    std::fs::read(path)
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|error| format!("could not read {}: {error}", path.display()))
}

fn strict_regular_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{} is a symlink — refusing", path.display()));
    }
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(())
}

pub(crate) fn load_fpm_manifest_strict(config_dir: &Path) -> Result<Option<Manifest>, String> {
    let manifest_dir = config_dir.join("fpm");
    match std::fs::symlink_metadata(&manifest_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not inspect FPM manifest directory {}: {error}",
                manifest_dir.display()
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "FPM manifest directory {} is a symlink — refusing",
                manifest_dir.display()
            ));
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(format!(
                "FPM manifest directory {} is not a directory",
                manifest_dir.display()
            ));
        }
        Ok(_) => {}
    }
    verify_user_channel(&manifest_dir, &[config_dir.to_path_buf()])
        .map_err(|error| format!("FPM manifest directory is not trusted: {error}"))?;

    let manifest_path = fpm_manifest_path(config_dir);
    match std::fs::symlink_metadata(&manifest_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not inspect FPM manifest {}: {error}",
                manifest_path.display()
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "FPM manifest {} is a symlink — refusing",
                manifest_path.display()
            ));
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(format!(
                "FPM manifest {} is not a regular file",
                manifest_path.display()
            ));
        }
        Ok(_) => {}
    }

    let manifest = Manifest::load(&manifest_path).map_err(|error| {
        format!(
            "FPM manifest {} is unreadable or malformed: {error}",
            manifest_path.display()
        )
    })?;
    if manifest.version != super::reconcile::MANIFEST_VERSION {
        return Err(format!(
            "unsupported FPM manifest version {} (expected {})",
            manifest.version,
            super::reconcile::MANIFEST_VERSION
        ));
    }
    if manifest.files.len() > 2 {
        return Err("FPM manifest contains more than the two allowed artifacts".to_string());
    }

    let expected_conf = conf_path(config_dir);
    let expected_probe = probe_script_path(config_dir);
    let mut saw_conf = false;
    let mut saw_probe = false;
    for entry in &manifest.files {
        if entry.php_version != "-" {
            return Err(format!(
                "forged FPM manifest entry {} has PHP version '{}' instead of '-'",
                entry.path.display(),
                entry.php_version
            ));
        }
        match (entry.path == expected_conf, entry.path == expected_probe) {
            (true, false) if entry.channel == "fpm-conf" && !saw_conf => saw_conf = true,
            (false, true) if entry.channel == "fpm-probe" && !saw_probe => saw_probe = true,
            (true, false) if saw_conf => {
                return Err("FPM manifest contains a duplicate config entry".to_string());
            }
            (false, true) if saw_probe => {
                return Err("FPM manifest contains a duplicate probe entry".to_string());
            }
            _ => {
                return Err(format!(
                    "forged FPM manifest entry ({}, {}) is outside the allowed artifact set",
                    entry.path.display(),
                    entry.channel
                ));
            }
        }
    }
    Ok(Some(manifest))
}

fn entry_owns_bytes(entry: &ManifestEntry, actual: &str) -> bool {
    entry.sha256 == actual
        || entry.expected_old_sha256.as_deref() == Some(actual)
        || entry.desired_sha256.as_deref() == Some(actual)
}

fn set_collision_preflight(manifest: &Manifest, paths: &[PathBuf]) -> Result<(), String> {
    for path in paths {
        let metadata = match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
            Ok(metadata) => metadata,
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(format!(
                "{} exists but is not a regular Hearth-owned artifact — remove or rename it, then run `hearth php config --sync`",
                path.display()
            ));
        }
        let actual = hash_file(path)?;
        let Some(entry) = manifest.files.iter().find(|entry| entry.path == *path) else {
            return Err(format!(
                "{} exists but is not tracked by the Hearth FPM manifest — remove or rename the file, then run `hearth php config --sync`",
                path.display()
            ));
        };
        if !entry_owns_bytes(entry, &actual) {
            return Err(format!(
                "{} was modified outside Hearth — remove or rename the file, then run `hearth php config --sync`",
                path.display()
            ));
        }
    }
    Ok(())
}

fn state_from_manifest(config_dir: &Path, manifest: Option<Manifest>) -> FpmConfState {
    let conf = conf_path(config_dir);
    let probe = probe_script_path(config_dir);
    let Some(manifest) = manifest else {
        if conf.exists() {
            return FpmConfState::UserManaged { conf };
        }
        if probe.exists() {
            return FpmConfState::Blocked {
                reason: format!(
                    "{} exists without an authoritative FPM manifest — remove or rename it, then run `hearth php config --sync`",
                    probe.display()
                ),
            };
        }
        return FpmConfState::Blocked {
            reason: "missing fpm config — run `hearth php config --sync`".to_string(),
        };
    };

    if manifest.files.is_empty() && conf.exists() {
        return FpmConfState::UserManaged { conf };
    }
    let conf_entry = manifest.files.iter().find(|entry| entry.path == conf);
    let probe_entry = manifest.files.iter().find(|entry| entry.path == probe);
    let (Some(conf_entry), Some(probe_entry)) = (conf_entry, probe_entry) else {
        return FpmConfState::Blocked {
            reason: "FPM manifest does not own the complete config + probe artifact set — run `hearth php config --sync`".to_string(),
        };
    };
    if conf_entry.state != EntryState::Applied || probe_entry.state != EntryState::Applied {
        return FpmConfState::Blocked {
            reason: "FPM manifest has an interrupted pending transaction — run `hearth php config --sync`".to_string(),
        };
    }
    for (entry, label) in [(conf_entry, "FPM config"), (probe_entry, "FPM probe")] {
        if let Err(reason) = strict_regular_file(&entry.path) {
            return FpmConfState::Blocked { reason };
        }
        match hash_file(&entry.path) {
            Ok(actual) if actual == entry.sha256 => {}
            Ok(_) => {
                return FpmConfState::Blocked {
                    reason: format!(
                        "{} {} was modified outside Hearth — remove or rename the file, then run `hearth php config --sync`",
                        label,
                        entry.path.display()
                    ),
                };
            }
            Err(reason) => return FpmConfState::Blocked { reason },
        }
    }
    FpmConfState::HearthOwned {
        conf,
        conf_sha256: conf_entry.sha256.clone(),
        probe_sha256: probe_entry.sha256.clone(),
        listen: socket_path(config_dir),
    }
}

pub fn conf_state(config_dir: &Path) -> FpmConfState {
    match load_fpm_manifest_strict(config_dir) {
        Ok(manifest) => state_from_manifest(config_dir, manifest),
        Err(reason) => FpmConfState::Blocked { reason },
    }
}

fn materialize_with_options(
    config_dir: &Path,
    hearth_root: &Path,
    template_version: u32,
    lock_wait: Duration,
) -> MaterializeReport {
    let rendered = match render_fpm_conf_v(template_version, config_dir) {
        Ok(rendered) => rendered,
        Err(reason) => return blocked_report(reason),
    };
    for dir in [
        config_dir.join("fpm"),
        config_dir.join("run"),
        config_dir.join("log"),
    ] {
        if let Err(reason) = ensure_private_dir(hearth_root, &dir) {
            return blocked_report(reason);
        }
    }

    let manifest_dir = config_dir.join("fpm");
    let manifest_path = fpm_manifest_path(config_dir);
    let conf = conf_path(config_dir);
    let probe = probe_script_path(config_dir);
    let verifier = HearthFpmDirVerifier {
        hearth_root,
        dir: &manifest_dir,
    };
    let transaction = manifest_set_transaction(
        &manifest_dir,
        &manifest_path,
        lock_wait,
        |path| {
            debug_assert_eq!(path, manifest_path);
            load_fpm_manifest_strict(config_dir).map(|manifest| {
                manifest.unwrap_or(Manifest {
                    version: super::reconcile::MANIFEST_VERSION,
                    files: Vec::new(),
                })
            })
        },
        FinalizeMode::DeleteWhenEmpty,
        |txn| {
            if let Err(reason) =
                set_collision_preflight(txn.manifest(), &[conf.clone(), probe.clone()])
            {
                let refusal = WriteOutcome::Refused {
                    reason: reason.clone(),
                };
                return (refusal.clone(), refusal);
            }
            let conf_spec = OwnedArtifactSpec {
                final_path: conf.clone(),
                channel: "fpm-conf".to_string(),
                php_version: "-".to_string(),
                mode: 0o600,
            };
            let probe_spec = OwnedArtifactSpec {
                final_path: probe.clone(),
                channel: "fpm-probe".to_string(),
                php_version: "-".to_string(),
                mode: 0o600,
            };
            let conf_outcome = txn.apply_owned_artifact(&conf_spec, rendered.as_bytes(), &verifier);
            if matches!(
                conf_outcome,
                WriteOutcome::Refused { .. } | WriteOutcome::Failed { .. }
            ) {
                return (conf_outcome.clone(), conf_outcome);
            }
            let probe_outcome =
                txn.apply_owned_artifact(&probe_spec, PROBE_SCRIPT_V1.as_bytes(), &verifier);
            (conf_outcome, probe_outcome)
        },
    );

    match transaction {
        Ok((conf_outcome, probe_outcome)) => MaterializeReport {
            conf: conf_outcome,
            probe: probe_outcome,
            state: conf_state(config_dir),
        },
        Err(error) => blocked_report(error.to_string()),
    }
}

pub fn materialize(config_dir: &Path, hearth_root: &Path) -> MaterializeReport {
    materialize_with_options(config_dir, hearth_root, 1, Duration::from_secs(5))
}

pub fn recover_pending_fpm(config_dir: &Path) -> anyhow::Result<()> {
    let manifest_dir = config_dir.join("fpm");
    if !manifest_dir.exists() {
        return Ok(());
    }
    let manifest_path = fpm_manifest_path(config_dir);
    manifest_set_transaction(
        &manifest_dir,
        &manifest_path,
        Duration::from_secs(5),
        |_| {
            load_fpm_manifest_strict(config_dir).map(|manifest| {
                manifest.unwrap_or(Manifest {
                    version: super::reconcile::MANIFEST_VERSION,
                    files: Vec::new(),
                })
            })
        },
        FinalizeMode::DeleteWhenEmpty,
        |_| (),
    )
    .map_err(anyhow::Error::from)
}

pub fn unmanage_fpm(config_dir: &Path) -> anyhow::Result<Vec<FileAction>> {
    let manifest_dir = config_dir.join("fpm");
    if !manifest_dir.exists() {
        return Ok(Vec::new());
    }
    let manifest_path = fpm_manifest_path(config_dir);
    manifest_set_transaction(
        &manifest_dir,
        &manifest_path,
        Duration::from_secs(5),
        |_| {
            load_fpm_manifest_strict(config_dir).map(|manifest| {
                manifest.unwrap_or(Manifest {
                    version: super::reconcile::MANIFEST_VERSION,
                    files: Vec::new(),
                })
            })
        },
        FinalizeMode::DeleteWhenEmpty,
        |txn| {
            let entries = txn.manifest().files.clone();
            let mut actions = Vec::new();
            for entry in entries {
                let outcome = txn.remove_owned_artifact(&entry.path);
                if !matches!(outcome, WriteOutcome::Unchanged) {
                    actions.push(FileAction {
                        path: entry.path,
                        php_version: entry.php_version,
                        channel: entry.channel,
                        outcome,
                    });
                }
            }
            actions
        },
    )
    .map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::*;

    fn config_dir_for_socket_len(socket_len: usize) -> PathBuf {
        let suffix_len = Path::new("/run/php-fpm.sock").as_os_str().as_bytes().len();
        let component_len = socket_len - 1 - suffix_len;
        PathBuf::from(format!("/{}", "a".repeat(component_len)))
    }

    #[test]
    fn render_is_byte_stable_and_contains_listener() {
        let config_dir = Path::new("/tmp/hearth config");
        let first = render_fpm_conf(config_dir).unwrap();
        let second = render_fpm_conf(config_dir).unwrap();

        assert_eq!(first, second);
        assert!(first.starts_with(FPM_CONF_HEADER_V1));
        assert!(first.contains("listen = /tmp/hearth config/run/php-fpm.sock"));
        assert!(first.contains("listen.mode = 0600"));
        assert!(first.contains("pm = ondemand"));
        assert!(first.contains("error_log = /tmp/hearth config/log/php-fpm.log"));
        assert!(first.contains("clear_env = yes"));
        assert!(first.contains("security.limit_extensions = .php"));
        assert!(!first.contains("php_admin_value"));
        assert!(!first.contains("user ="));
    }

    #[test]
    fn render_refuses_oversized_socket_path_byte_exact() {
        let at_limit = config_dir_for_socket_len(103);
        assert_eq!(socket_path(&at_limit).as_os_str().as_bytes().len(), 103);
        assert!(render_fpm_conf(&at_limit).is_ok());

        let too_long = config_dir_for_socket_len(104);
        assert_eq!(socket_path(&too_long).as_os_str().as_bytes().len(), 104);
        let error = render_fpm_conf(&too_long).unwrap_err();
        assert!(error.contains("104 bytes"), "{error}");
        assert!(error.contains("limit 103 + NUL"), "{error}");
        assert!(error.contains(&too_long.display().to_string()), "{error}");
    }

    #[test]
    fn render_refuses_multibyte_overflow() {
        let config_dir = PathBuf::from(format!("/{}", "é".repeat(50)));
        let listener = socket_path(&config_dir);
        assert!(listener.to_string_lossy().chars().count() < 104);
        assert!(listener.as_os_str().as_bytes().len() > 103);
        assert!(render_fpm_conf(&config_dir).is_err());
    }

    #[test]
    fn render_refuses_crlf_bearing_paths() {
        for config_dir in [
            PathBuf::from("/tmp/hearth\nconfig"),
            PathBuf::from("/tmp/hearth\rconfig"),
        ] {
            let error = render_fpm_conf(&config_dir).unwrap_err();
            assert!(error.contains("must not contain CR, LF, or NUL"), "{error}");
        }

        let config_dir = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/hearth\0config".to_vec(),
        ));
        let error = render_fpm_conf(&config_dir).unwrap_err();
        assert!(error.contains("must not contain CR, LF, or NUL"), "{error}");
    }

    #[test]
    fn paths_derive_only_from_config_dir() {
        let config_dir = Path::new("/tmp/hearth config");
        assert_eq!(socket_path(config_dir), config_dir.join("run/php-fpm.sock"));
        assert_eq!(conf_path(config_dir), config_dir.join("fpm/php-fpm.conf"));
        assert_eq!(
            probe_script_path(config_dir),
            config_dir.join("fpm/hearth-probe.php")
        );
        assert_eq!(
            fpm_manifest_path(config_dir),
            config_dir.join("fpm/manifest.toml")
        );
        assert!(PROBE_SCRIPT_V1.contains("json_encode"));
        assert!(PROBE_SCRIPT_V1.contains("getmypid"));
        assert!(PROBE_SCRIPT_V1.contains("Content-Type"));
    }

    fn fpm_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let config_dir = base.join("hearth");
        std::fs::create_dir(&config_dir).unwrap();
        (tmp, base, config_dir)
    }

    fn assert_private_dir(path: &Path) {
        assert!(path.is_dir(), "missing directory {}", path.display());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    fn manifest_entry(
        path: PathBuf,
        channel: &str,
        bytes: &[u8],
    ) -> crate::php::reconcile::ManifestEntry {
        crate::php::reconcile::ManifestEntry {
            path,
            php_version: "-".to_string(),
            channel: channel.to_string(),
            sha256: crate::php::reconcile::sha256_hex(bytes),
            state: crate::php::reconcile::EntryState::Applied,
            expected_old_sha256: None,
            desired_sha256: None,
            last_outcome: crate::php::reconcile::LastOutcome::Written,
            applied_at_unix_ms: Some(1),
        }
    }

    #[test]
    fn materialize_creates_dirs_conf_and_probe_atomically() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        let report = materialize(&config_dir, &config_dir);
        assert_eq!(report.conf, crate::php::reconcile::WriteOutcome::Written);
        assert_eq!(report.probe, crate::php::reconcile::WriteOutcome::Written);
        assert!(matches!(report.state, FpmConfState::HearthOwned { .. }));
        for dir in [
            config_dir.join("fpm"),
            config_dir.join("run"),
            config_dir.join("log"),
        ] {
            assert_private_dir(&dir);
        }
        for file in [conf_path(&config_dir), probe_script_path(&config_dir)] {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let manifest = load_fpm_manifest_strict(&config_dir).unwrap().unwrap();
        assert_eq!(manifest.files.len(), 2);
        assert!(manifest.files.iter().all(|entry| {
            entry.state == crate::php::reconcile::EntryState::Applied
                && entry.applied_at_unix_ms.is_some()
        }));
    }

    #[test]
    fn materialize_is_idempotent_second_run_unchanged() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        let first = materialize(&config_dir, &config_dir);
        assert!(matches!(first.state, FpmConfState::HearthOwned { .. }));
        let conf_before = std::fs::metadata(conf_path(&config_dir))
            .unwrap()
            .modified()
            .unwrap();
        let probe_before = std::fs::metadata(probe_script_path(&config_dir))
            .unwrap()
            .modified()
            .unwrap();
        let conf_bytes = std::fs::read(conf_path(&config_dir)).unwrap();
        let probe_bytes = std::fs::read(probe_script_path(&config_dir)).unwrap();

        let second = materialize(&config_dir, &config_dir);
        assert_eq!(second.conf, crate::php::reconcile::WriteOutcome::Unchanged);
        assert_eq!(second.probe, crate::php::reconcile::WriteOutcome::Unchanged);
        assert_eq!(std::fs::read(conf_path(&config_dir)).unwrap(), conf_bytes);
        assert_eq!(
            std::fs::read(probe_script_path(&config_dir)).unwrap(),
            probe_bytes
        );
        assert_eq!(
            std::fs::metadata(conf_path(&config_dir))
                .unwrap()
                .modified()
                .unwrap(),
            conf_before
        );
        assert_eq!(
            std::fs::metadata(probe_script_path(&config_dir))
                .unwrap()
                .modified()
                .unwrap(),
            probe_before
        );
    }

    #[test]
    fn foreign_conf_preflight_blocks_all_writes() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        std::fs::create_dir_all(config_dir.join("fpm")).unwrap();
        let foreign = b"; smoke fpm config\n";
        std::fs::write(conf_path(&config_dir), foreign).unwrap();
        let report = materialize(&config_dir, &config_dir);
        assert!(matches!(report.state, FpmConfState::UserManaged { .. }));
        assert_eq!(std::fs::read(conf_path(&config_dir)).unwrap(), foreign);
        assert!(!probe_script_path(&config_dir).exists());
        assert!(!fpm_manifest_path(&config_dir).exists());
    }

    #[test]
    fn foreign_probe_collision_blocks_all_writes() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        std::fs::create_dir_all(config_dir.join("fpm")).unwrap();
        let foreign = b"foreign probe\n";
        std::fs::write(probe_script_path(&config_dir), foreign).unwrap();
        let report = materialize(&config_dir, &config_dir);
        assert!(matches!(report.state, FpmConfState::Blocked { .. }));
        assert!(!conf_path(&config_dir).exists());
        assert_eq!(
            std::fs::read(probe_script_path(&config_dir)).unwrap(),
            foreign
        );
        assert!(!fpm_manifest_path(&config_dir).exists());
    }

    #[test]
    fn template_upgrade_regenerates_via_version_seam() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        let first = materialize_with_options(
            &config_dir,
            &config_dir,
            1,
            std::time::Duration::from_secs(1),
        );
        assert!(matches!(first.state, FpmConfState::HearthOwned { .. }));
        let before = load_fpm_manifest_strict(&config_dir)
            .unwrap()
            .unwrap()
            .files
            .into_iter()
            .find(|entry| entry.path == conf_path(&config_dir))
            .unwrap()
            .applied_at_unix_ms
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let upgraded = materialize_with_options(
            &config_dir,
            &config_dir,
            2,
            std::time::Duration::from_secs(1),
        );
        assert_eq!(upgraded.conf, crate::php::reconcile::WriteOutcome::Written);
        assert!(
            std::fs::read_to_string(conf_path(&config_dir))
                .unwrap()
                .contains("fpm-conf v2")
        );
        let after = load_fpm_manifest_strict(&config_dir)
            .unwrap()
            .unwrap()
            .files
            .into_iter()
            .find(|entry| entry.path == conf_path(&config_dir))
            .unwrap()
            .applied_at_unix_ms
            .unwrap();
        assert!(after > before);
    }

    #[test]
    fn tampered_owned_conf_is_refused_with_remediation() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        materialize(&config_dir, &config_dir);
        std::fs::write(conf_path(&config_dir), b"tampered\n").unwrap();
        let report = materialize(&config_dir, &config_dir);
        match report.state {
            FpmConfState::Blocked { reason } => assert!(reason.contains("modified outside Hearth")),
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(conf_path(&config_dir)).unwrap(),
            b"tampered\n"
        );
    }

    fn assert_forged_manifest_blocked(mut manifest: crate::php::reconcile::Manifest) {
        let (_tmp, _base, config_dir) = fpm_fixture();
        std::fs::create_dir_all(config_dir.join("fpm")).unwrap();
        for entry in &mut manifest.files {
            if entry.path == Path::new("CONF") {
                entry.path = conf_path(&config_dir);
            } else if entry.path == Path::new("PROBE") {
                entry.path = probe_script_path(&config_dir);
            }
        }
        manifest.save(&fpm_manifest_path(&config_dir)).unwrap();
        let report = materialize(&config_dir, &config_dir);
        assert!(matches!(report.state, FpmConfState::Blocked { .. }));
        assert!(!conf_path(&config_dir).exists());
        assert!(!probe_script_path(&config_dir).exists());
    }

    #[test]
    fn forged_manifest_is_blocked_zero_mutation() {
        let base_entry = manifest_entry(PathBuf::from("CONF"), "fpm-conf", b"x");
        let mut outside = base_entry.clone();
        outside.path = PathBuf::from("/tmp/hearth-forged-outside");
        assert_forged_manifest_blocked(crate::php::reconcile::Manifest {
            version: crate::php::reconcile::MANIFEST_VERSION,
            files: vec![outside],
        });

        assert_forged_manifest_blocked(crate::php::reconcile::Manifest {
            version: crate::php::reconcile::MANIFEST_VERSION,
            files: vec![base_entry.clone(), base_entry.clone()],
        });
        assert_forged_manifest_blocked(crate::php::reconcile::Manifest {
            version: crate::php::reconcile::MANIFEST_VERSION,
            files: vec![
                base_entry.clone(),
                manifest_entry(PathBuf::from("PROBE"), "fpm-probe", b"y"),
                manifest_entry(PathBuf::from("/tmp/third"), "fpm-probe", b"z"),
            ],
        });
        let mut wrong_channel = base_entry.clone();
        wrong_channel.channel = "wrong".to_string();
        assert_forged_manifest_blocked(crate::php::reconcile::Manifest {
            version: crate::php::reconcile::MANIFEST_VERSION,
            files: vec![wrong_channel],
        });
        assert_forged_manifest_blocked(crate::php::reconcile::Manifest {
            version: 999,
            files: vec![base_entry],
        });

        let (_tmp, _base, config_dir) = fpm_fixture();
        std::fs::create_dir_all(config_dir.join("fpm")).unwrap();
        let target = config_dir.join("foreign-manifest.toml");
        std::fs::write(&target, "version = 1\nfiles = []\n").unwrap();
        std::os::unix::fs::symlink(&target, fpm_manifest_path(&config_dir)).unwrap();
        let report = materialize(&config_dir, &config_dir);
        assert!(matches!(report.state, FpmConfState::Blocked { .. }));
        assert!(!conf_path(&config_dir).exists());
    }

    #[test]
    fn symlinked_fpm_dir_is_refused_zero_mutation_and_oversized_is_typed() {
        let (_tmp, base, config_dir) = fpm_fixture();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, config_dir.join("fpm")).unwrap();
        let report = materialize(&config_dir, &config_dir);
        assert!(matches!(report.state, FpmConfState::Blocked { .. }));
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());

        let too_long = config_dir_for_socket_len(104);
        let report = materialize(&too_long, &too_long);
        match report.state {
            FpmConfState::Blocked { reason } => assert!(reason.contains("limit 103 + NUL")),
            other => panic!("expected typed Blocked, got {other:?}"),
        }
        assert!(!too_long.exists());
    }

    #[test]
    fn crash_recovery_replays_pending_fpm_journal() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        materialize(&config_dir, &config_dir);
        let conf = conf_path(&config_dir);
        let mut manifest = load_fpm_manifest_strict(&config_dir).unwrap().unwrap();
        let entry = manifest
            .files
            .iter_mut()
            .find(|entry| entry.path == conf)
            .unwrap();
        let old = entry.sha256.clone();
        let desired = render_fpm_conf_v(2, &config_dir).unwrap();
        entry.state = crate::php::reconcile::EntryState::Pending;
        entry.expected_old_sha256 = Some(old);
        entry.desired_sha256 = Some(crate::php::reconcile::sha256_hex(desired.as_bytes()));
        manifest.save(&fpm_manifest_path(&config_dir)).unwrap();

        recover_pending_fpm(&config_dir).unwrap();
        let still_pending = load_fpm_manifest_strict(&config_dir).unwrap().unwrap();
        assert_eq!(
            still_pending
                .files
                .iter()
                .find(|entry| entry.path == conf)
                .unwrap()
                .state,
            crate::php::reconcile::EntryState::Pending
        );
        let report = materialize_with_options(
            &config_dir,
            &config_dir,
            2,
            std::time::Duration::from_secs(1),
        );
        assert!(matches!(report.state, FpmConfState::HearthOwned { .. }));

        let mut deleting = load_fpm_manifest_strict(&config_dir).unwrap().unwrap();
        let probe = probe_script_path(&config_dir);
        let probe_entry = deleting
            .files
            .iter_mut()
            .find(|entry| entry.path == probe)
            .unwrap();
        probe_entry.state = crate::php::reconcile::EntryState::Pending;
        probe_entry.expected_old_sha256 = Some(probe_entry.sha256.clone());
        probe_entry.desired_sha256 = None;
        deleting.save(&fpm_manifest_path(&config_dir)).unwrap();
        std::fs::remove_file(&probe).unwrap();
        recover_pending_fpm(&config_dir).unwrap();
        assert!(
            load_fpm_manifest_strict(&config_dir)
                .unwrap()
                .unwrap()
                .files
                .iter()
                .all(|entry| entry.path != probe)
        );
    }

    #[test]
    fn unmanage_fpm_removes_only_owned_exact_hash() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        materialize(&config_dir, &config_dir);
        let actions = unmanage_fpm(&config_dir).unwrap();
        assert_eq!(actions.len(), 2);
        assert!(
            actions.iter().all(|action| matches!(
                action.outcome,
                crate::php::reconcile::WriteOutcome::Deleted
            ))
        );
        assert!(!conf_path(&config_dir).exists());
        assert!(!probe_script_path(&config_dir).exists());
        assert!(!fpm_manifest_path(&config_dir).exists());

        let foreign = b"foreign\n";
        std::fs::write(conf_path(&config_dir), foreign).unwrap();
        let actions = unmanage_fpm(&config_dir).unwrap();
        assert!(actions.is_empty());
        assert_eq!(std::fs::read(conf_path(&config_dir)).unwrap(), foreign);
    }

    fn wait_for_flag(path: &Path) {
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

    fn fpm_child(role: &str, config_dir: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("fpm_txn_child_role")
            .arg("--nocapture")
            .env("HEARTH_FPM_TXN_ROLE", role)
            .env("HEARTH_FPM_TXN_FIXTURE", config_dir);
        command
    }

    fn assert_child_success(output: std::process::Output) {
        assert!(
            output.status.success(),
            "child failed: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn fpm_txn_child_role() {
        let Some(role) = std::env::var_os("HEARTH_FPM_TXN_ROLE") else {
            return;
        };
        let config_dir = PathBuf::from(std::env::var_os("HEARTH_FPM_TXN_FIXTURE").unwrap());
        if let Some(started) = std::env::var_os("HEARTH_FPM_CHILD_STARTED") {
            std::fs::write(started, b"started").unwrap();
        }
        match role.to_string_lossy().as_ref() {
            "materialize" => {
                let version = std::env::var("HEARTH_FPM_TEMPLATE_VERSION")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(1);
                let wait = std::env::var("HEARTH_FPM_LOCK_WAIT_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(std::time::Duration::from_secs(5));
                let report = materialize_with_options(&config_dir, &config_dir, version, wait);
                if std::env::var_os("HEARTH_FPM_EXPECT_BUSY").is_some() {
                    assert!(
                        matches!(report.state, FpmConfState::Blocked { ref reason } if reason.contains("another Hearth process"))
                    );
                } else {
                    assert!(matches!(report.state, FpmConfState::HearthOwned { .. }));
                }
                if std::env::var_os("HEARTH_FPM_EXPECT_UNCHANGED").is_some() {
                    assert_eq!(report.conf, crate::php::reconcile::WriteOutcome::Unchanged);
                    assert_eq!(report.probe, crate::php::reconcile::WriteOutcome::Unchanged);
                }
            }
            "unmanage" => {
                unmanage_fpm(&config_dir).unwrap();
            }
            "hold_lock" => {
                let lock = crate::php::reconcile::acquire_manifest_lock(
                    &config_dir.join("fpm"),
                    std::time::Duration::from_secs(1),
                )
                .unwrap();
                let _lock = lock;
                let prefix = config_dir.join("barrier/hold");
                std::fs::create_dir_all(prefix.parent().unwrap()).unwrap();
                std::fs::write(prefix.with_extension("entered"), b"entered").unwrap();
                wait_for_flag(&prefix.with_extension("release"));
            }
            other => panic!("unknown child role {other}"),
        }
    }

    #[test]
    fn cross_process_unmanage_vs_materialize_cannot_interleave() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        materialize(&config_dir, &config_dir);
        let prefix = config_dir.join("barrier/unmanage");
        let mut unmanage = fpm_child("unmanage", &config_dir);
        unmanage.env("HEARTH_TXN_PAUSE_AFTER_JOURNAL", &prefix);
        let mut unmanage = unmanage.spawn().unwrap();
        wait_for_flag(&prefix.with_extension("entered"));

        let conf_before = std::fs::read(conf_path(&config_dir)).unwrap();
        let probe_before = std::fs::read(probe_script_path(&config_dir)).unwrap();
        let output = fpm_child("materialize", &config_dir)
            .env("HEARTH_FPM_LOCK_WAIT_MS", "200")
            .env("HEARTH_FPM_EXPECT_BUSY", "1")
            .output()
            .unwrap();
        assert_child_success(output);
        assert_eq!(std::fs::read(conf_path(&config_dir)).unwrap(), conf_before);
        assert_eq!(
            std::fs::read(probe_script_path(&config_dir)).unwrap(),
            probe_before
        );

        std::fs::write(prefix.with_extension("release"), b"release").unwrap();
        assert!(unmanage.wait().unwrap().success());
        assert!(!conf_path(&config_dir).exists());
        assert!(!probe_script_path(&config_dir).exists());
        assert!(!fpm_manifest_path(&config_dir).exists());
        assert_child_success(fpm_child("materialize", &config_dir).output().unwrap());
        assert!(matches!(
            conf_state(&config_dir),
            FpmConfState::HearthOwned { .. }
        ));
    }

    #[test]
    fn cross_process_materialize_vs_materialize_serialize() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        let prefix = config_dir.join("barrier/materialize");
        let mut first = fpm_child("materialize", &config_dir);
        first.env("HEARTH_TXN_PAUSE_AFTER_JOURNAL", &prefix);
        let mut first = first.spawn().unwrap();
        wait_for_flag(&prefix.with_extension("entered"));

        let started = config_dir.join("barrier/second.started");
        let mut second = fpm_child("materialize", &config_dir);
        second
            .env("HEARTH_FPM_CHILD_STARTED", &started)
            .env("HEARTH_FPM_EXPECT_UNCHANGED", "1");
        let mut second = second.spawn().unwrap();
        wait_for_flag(&started);
        std::fs::write(prefix.with_extension("release"), b"release").unwrap();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        assert!(matches!(
            conf_state(&config_dir),
            FpmConfState::HearthOwned { .. }
        ));
    }

    #[test]
    fn crash_while_holding_lock_releases_and_recovers() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        materialize(&config_dir, &config_dir);
        let prefix = config_dir.join("barrier/crash");
        let mut child = fpm_child("materialize", &config_dir);
        child
            .env("HEARTH_FPM_TEMPLATE_VERSION", "2")
            .env("HEARTH_TXN_PAUSE_AFTER_JOURNAL", &prefix)
            .env("HEARTH_TXN_ABORT_AFTER_JOURNAL", "1");
        let status = child.status().unwrap();
        assert!(!status.success());
        wait_for_flag(&prefix.with_extension("entered"));
        let report = materialize_with_options(
            &config_dir,
            &config_dir,
            2,
            std::time::Duration::from_secs(1),
        );
        assert!(matches!(report.state, FpmConfState::HearthOwned { .. }));
    }

    #[test]
    fn lock_busy_yields_conservative_refusal_zero_mutation() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        materialize(&config_dir, &config_dir);
        let conf_before = std::fs::read(conf_path(&config_dir)).unwrap();
        let manifest_before = std::fs::read(fpm_manifest_path(&config_dir)).unwrap();
        let prefix = config_dir.join("barrier/hold");
        let mut holder = fpm_child("hold_lock", &config_dir).spawn().unwrap();
        wait_for_flag(&prefix.with_extension("entered"));
        let report = materialize_with_options(
            &config_dir,
            &config_dir,
            2,
            std::time::Duration::from_millis(100),
        );
        assert!(
            matches!(report.state, FpmConfState::Blocked { ref reason } if reason.contains("another Hearth process"))
        );
        assert_eq!(std::fs::read(conf_path(&config_dir)).unwrap(), conf_before);
        assert_eq!(
            std::fs::read(fpm_manifest_path(&config_dir)).unwrap(),
            manifest_before
        );
        std::fs::write(prefix.with_extension("release"), b"release").unwrap();
        assert!(holder.wait().unwrap().success());
    }

    #[test]
    fn empty_manifest_deletion_is_durable_and_idempotent() {
        let (_tmp, _base, config_dir) = fpm_fixture();
        materialize(&config_dir, &config_dir);
        let prefix = config_dir.join("barrier/unlink");
        let mut child = fpm_child("unmanage", &config_dir);
        child
            .env("HEARTH_TXN_PAUSE_BEFORE_MANIFEST_UNLINK", &prefix)
            .env("HEARTH_TXN_ABORT_AFTER_JOURNAL", "1");
        let status = child.status().unwrap();
        assert!(!status.success());
        wait_for_flag(&prefix.with_extension("entered"));
        assert!(!conf_path(&config_dir).exists());
        assert!(!probe_script_path(&config_dir).exists());
        assert!(fpm_manifest_path(&config_dir).exists());
        assert!(unmanage_fpm(&config_dir).unwrap().is_empty());
        assert!(!fpm_manifest_path(&config_dir).exists());
        assert!(unmanage_fpm(&config_dir).unwrap().is_empty());
    }

    #[test]
    fn downgrade_isolation_ini_reconcile_never_touches_fpm_artifacts() {
        let (_tmp, base, config_dir) = fpm_fixture();
        materialize(&config_dir, &config_dir);
        let conf_before = std::fs::read(conf_path(&config_dir)).unwrap();
        let probe_before = std::fs::read(probe_script_path(&config_dir)).unwrap();
        let manifest_before = std::fs::read(fpm_manifest_path(&config_dir)).unwrap();
        let roots = crate::php::targets::ProviderRoots::isolated(
            &base,
            config_dir.clone(),
            base.join("Herd"),
            base.join("homebrew"),
        )
        .unwrap();
        let report = crate::php::reconcile::reconcile(
            &crate::config::PhpIniSettings::default(),
            &[],
            &config_dir.join("php/manifest.toml"),
            &roots,
        );
        assert!(report.files.is_empty());
        assert_eq!(std::fs::read(conf_path(&config_dir)).unwrap(), conf_before);
        assert_eq!(
            std::fs::read(probe_script_path(&config_dir)).unwrap(),
            probe_before
        );
        assert_eq!(
            std::fs::read(fpm_manifest_path(&config_dir)).unwrap(),
            manifest_before
        );
    }
}
