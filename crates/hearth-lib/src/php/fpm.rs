use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
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
}
