use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context;
use tracing::{info, warn};

use crate::service::manager::is_herd_running;

const HERD_CLI_FROM_CONFIG_DIR: &str = "Herd/bin/herd";
const HERD_CLI_FROM_HOME_DIR: &str = "Library/Application Support/Herd/bin/herd";

/// Compatibility wrapper for the site-management CLI.
///
/// Valet remains the default backend. When Herd is running, Hearth delegates
/// the site commands that Herd owns to Herd's CLI instead, because a
/// Herd-managed Valet installation hides those commands.
///
/// Isolate deliberately remains Valet-only: Herd's CLI does not expose a
/// compatible isolate command.
pub struct ValetCli;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendPolicy {
    HerdAware,
    ValetOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SiteBackendKind {
    Valet,
    Herd,
}

impl SiteBackendKind {
    fn display_name(self) -> &'static str {
        match self {
            Self::Valet => "Valet",
            Self::Herd => "Herd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SiteCliBackend {
    kind: SiteBackendKind,
    executable: PathBuf,
}

impl SiteCliBackend {
    fn valet() -> Self {
        Self {
            kind: SiteBackendKind::Valet,
            executable: PathBuf::from("valet"),
        }
    }

    fn select(
        policy: BackendPolicy,
        herd_running: bool,
        config_dir: Option<&Path>,
        home_dir: Option<&Path>,
    ) -> anyhow::Result<Self> {
        if policy == BackendPolicy::ValetOnly || !herd_running {
            return Ok(Self::valet());
        }

        let mut candidates = Vec::new();
        if let Some(config_dir) = config_dir {
            candidates.push(config_dir.join(HERD_CLI_FROM_CONFIG_DIR));
        }
        if let Some(home_dir) = home_dir {
            let candidate = home_dir.join(HERD_CLI_FROM_HOME_DIR);
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }

        if let Some(executable) = candidates.iter().find(|path| path.is_file()) {
            return Ok(Self {
                kind: SiteBackendKind::Herd,
                executable: executable.clone(),
            });
        }

        if candidates.is_empty() {
            anyhow::bail!(
                "Herd is running, but the Herd CLI location could not be resolved from the user's config or home directory"
            );
        }

        let searched = candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!("Herd is running, but the Herd CLI was not found. Searched: {searched}");
    }

    fn run<I, S>(
        &self,
        operation: &str,
        args: I,
        current_dir: Option<&Path>,
    ) -> anyhow::Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.executable);
        command.arg(operation).args(args);
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }

        let backend = self.kind.display_name();
        let output = command
            .output()
            .with_context(|| format!("Failed to run {backend} {operation}"))?;

        if !output.status.success() {
            let stderr = filter_stderr(&output);
            anyhow::bail!("{backend} {operation} failed: {stderr}");
        }

        Ok(output)
    }
}

fn selected_backend(policy: BackendPolicy) -> anyhow::Result<SiteCliBackend> {
    if policy == BackendPolicy::ValetOnly {
        return Ok(SiteCliBackend::valet());
    }

    let config_dir = dirs::config_dir();
    let home_dir = dirs::home_dir();
    SiteCliBackend::select(
        policy,
        is_herd_running(),
        config_dir.as_deref(),
        home_dir.as_deref(),
    )
}

/// Filter PHP deprecation warnings from stderr.
///
/// Valet's PHP code triggers deprecation warnings on PHP 8.4+ (implicit
/// nullable parameters). These are harmless noise — strip them so users
/// only see real errors.
fn filter_stderr(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .filter(|line| !line.starts_with("Deprecated:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

impl ValetCli {
    /// Check if Valet is installed and available.
    pub fn is_installed() -> bool {
        Command::new("valet")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Install Valet via Composer if not already installed.
    pub fn install() -> anyhow::Result<()> {
        if Self::is_installed() {
            info!("Valet is already installed");
            return Ok(());
        }

        info!("Installing Laravel Valet via Composer...");
        let output = Command::new("composer")
            .args(["global", "require", "laravel/valet"])
            .output()
            .context("Failed to run composer. Is Composer installed?")?;

        if !output.status.success() {
            let stderr = filter_stderr(&output);
            anyhow::bail!("Valet installation failed: {}", stderr);
        }

        // Run valet install to set up Nginx, dnsmasq, etc.
        let output = Command::new("valet")
            .arg("install")
            .output()
            .context("Failed to run valet install")?;

        if !output.status.success() {
            let stderr = filter_stderr(&output);
            warn!("valet install had issues: {}", stderr);
        }

        Ok(())
    }

    /// Link the current directory as a site.
    pub fn link(name: Option<&str>) -> anyhow::Result<String> {
        let backend = selected_backend(BackendPolicy::HerdAware)?;
        Self::link_with_backend(&backend, name)
    }

    fn link_with_backend(backend: &SiteCliBackend, name: Option<&str>) -> anyhow::Result<String> {
        let output = backend.run("link", name, None)?;
        Ok(stdout(&output))
    }

    /// Link a directory as a site, running the command in the given directory.
    ///
    /// Unlike link(), this does not mutate the process's current directory.
    /// Safe for concurrent use from the MCP server.
    pub fn link_in(working_dir: &str, name: Option<&str>) -> anyhow::Result<String> {
        let backend = selected_backend(BackendPolicy::HerdAware)?;
        Self::link_in_with_backend(&backend, Path::new(working_dir), name)
    }

    fn link_in_with_backend(
        backend: &SiteCliBackend,
        working_dir: &Path,
        name: Option<&str>,
    ) -> anyhow::Result<String> {
        let output = backend.run("link", name, Some(working_dir))?;
        Ok(stdout(&output))
    }

    /// Unlink a site.
    pub fn unlink(name: &str) -> anyhow::Result<()> {
        let backend = selected_backend(BackendPolicy::HerdAware)?;
        Self::unlink_with_backend(&backend, name)
    }

    fn unlink_with_backend(backend: &SiteCliBackend, name: &str) -> anyhow::Result<()> {
        backend.run("unlink", [name], None)?;
        Ok(())
    }

    /// Park a directory (all subdirectories become sites).
    pub fn park(path: &str) -> anyhow::Result<String> {
        let backend = selected_backend(BackendPolicy::HerdAware)?;
        Self::park_with_backend(&backend, Path::new(path))
    }

    fn park_with_backend(backend: &SiteCliBackend, working_dir: &Path) -> anyhow::Result<String> {
        let output = backend.run("park", std::iter::empty::<&str>(), Some(working_dir))?;
        Ok(stdout(&output))
    }

    /// Secure a site with SSL.
    pub fn secure(name: &str) -> anyhow::Result<()> {
        let backend = selected_backend(BackendPolicy::HerdAware)?;
        Self::secure_with_backend(&backend, name)
    }

    fn secure_with_backend(backend: &SiteCliBackend, name: &str) -> anyhow::Result<()> {
        backend.run("secure", [name], None)?;
        Ok(())
    }

    /// Unsecure a site.
    pub fn unsecure(name: &str) -> anyhow::Result<()> {
        let backend = selected_backend(BackendPolicy::HerdAware)?;
        Self::unsecure_with_backend(&backend, name)
    }

    fn unsecure_with_backend(backend: &SiteCliBackend, name: &str) -> anyhow::Result<()> {
        backend.run("unsecure", [name], None)?;
        Ok(())
    }

    /// Isolate a site to a specific PHP version.
    ///
    /// This remains Valet-only because Herd does not expose a compatible
    /// isolate command. Keeping it on Valet preserves the existing argv
    /// contract instead of guessing at unsupported Herd behavior.
    pub fn isolate(name: &str, php_version: &str) -> anyhow::Result<()> {
        let backend = selected_backend(BackendPolicy::ValetOnly)?;
        Self::isolate_with_backend(&backend, name, php_version)
    }

    fn isolate_with_backend(
        backend: &SiteCliBackend,
        name: &str,
        php_version: &str,
    ) -> anyhow::Result<()> {
        backend.run(
            "isolate",
            [
                name.to_string(),
                format!("--site={name}"),
                format!("php@{php_version}"),
            ],
            None,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct FakeCli {
        _temp: tempfile::TempDir,
        config_dir: PathBuf,
        backend: SiteCliBackend,
    }

    impl FakeCli {
        fn new(kind: SiteBackendKind) -> Self {
            let temp = tempfile::TempDir::new().unwrap();
            let config_dir = temp.path().join("Application Support");
            let executable = config_dir.join(HERD_CLI_FROM_CONFIG_DIR);
            write_fake_cli(&executable);

            Self {
                _temp: temp,
                config_dir,
                backend: SiteCliBackend { kind, executable },
            }
        }

        fn args(&self) -> Vec<String> {
            fs::read_to_string(self.backend.executable.parent().unwrap().join("args"))
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn cwd(&self) -> PathBuf {
            PathBuf::from(
                fs::read_to_string(self.backend.executable.parent().unwrap().join("cwd"))
                    .unwrap()
                    .trim(),
            )
        }

        fn fail_with(&self, message: &str) {
            fs::write(
                self.backend.executable.parent().unwrap().join("fail"),
                message,
            )
            .unwrap();
        }
    }

    fn write_fake_cli(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            r#"#!/bin/sh
log_dir=${0%/*}
printf '%s\n' "$PWD" > "$log_dir/cwd"
printf '%s\n' "$@" > "$log_dir/args"
if [ -f "$log_dir/fail" ]; then
    printf 'Deprecated: filtered warning\n' >&2
    cat "$log_dir/fail" >&2
    exit 7
fi
printf '  fake success output  \n'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn selects_valet_when_herd_is_not_running() {
        let backend = SiteCliBackend::select(BackendPolicy::HerdAware, false, None, None).unwrap();

        assert_eq!(backend.kind, SiteBackendKind::Valet);
        assert_eq!(backend.executable, PathBuf::from("valet"));
    }

    #[test]
    fn selects_herd_from_config_dir_with_spaces() {
        let fake = FakeCli::new(SiteBackendKind::Herd);
        let backend =
            SiteCliBackend::select(BackendPolicy::HerdAware, true, Some(&fake.config_dir), None)
                .unwrap();

        assert_eq!(backend, fake.backend);
        assert!(backend.executable.to_string_lossy().contains(' '));
    }

    #[test]
    fn selects_herd_from_home_when_config_candidate_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path().join("missing config");
        let home_dir = temp.path().join("home directory");
        let executable = home_dir.join(HERD_CLI_FROM_HOME_DIR);
        write_fake_cli(&executable);

        let backend = SiteCliBackend::select(
            BackendPolicy::HerdAware,
            true,
            Some(&config_dir),
            Some(&home_dir),
        )
        .unwrap();

        assert_eq!(backend.kind, SiteBackendKind::Herd);
        assert_eq!(backend.executable, executable);
    }

    #[test]
    fn missing_herd_cli_reports_the_herd_backend_and_searched_paths() {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path().join("Application Support");
        let error = SiteCliBackend::select(BackendPolicy::HerdAware, true, Some(&config_dir), None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Herd is running"));
        assert!(
            error.contains(
                &config_dir
                    .join(HERD_CLI_FROM_CONFIG_DIR)
                    .display()
                    .to_string()
            )
        );
    }

    #[test]
    fn link_preserves_optional_name_and_inherited_current_dir() {
        let fake = FakeCli::new(SiteBackendKind::Herd);
        let output = ValetCli::link_with_backend(&fake.backend, None).unwrap();

        assert_eq!(output, "fake success output");
        assert_eq!(fake.args(), ["link"]);
        assert_eq!(fake.cwd(), std::env::current_dir().unwrap());

        ValetCli::link_with_backend(&fake.backend, Some("custom-name")).unwrap();
        assert_eq!(fake.args(), ["link", "custom-name"]);
    }

    #[test]
    fn link_in_preserves_optional_name_and_uses_requested_current_dir() {
        let fake = FakeCli::new(SiteBackendKind::Herd);
        let working_dir = fake.config_dir.join("site with spaces");
        fs::create_dir_all(&working_dir).unwrap();

        ValetCli::link_in_with_backend(&fake.backend, &working_dir, None).unwrap();
        assert_eq!(fake.args(), ["link"]);
        assert_eq!(
            fake.cwd(),
            working_dir
                .canonicalize()
                .expect("working dir should exist")
        );

        ValetCli::link_in_with_backend(&fake.backend, &working_dir, Some("custom-name")).unwrap();
        assert_eq!(fake.args(), ["link", "custom-name"]);
    }

    #[test]
    fn unlink_secure_and_unsecure_map_commands_and_names() {
        let fake = FakeCli::new(SiteBackendKind::Herd);

        ValetCli::unlink_with_backend(&fake.backend, "demo").unwrap();
        assert_eq!(fake.args(), ["unlink", "demo"]);

        ValetCli::secure_with_backend(&fake.backend, "demo").unwrap();
        assert_eq!(fake.args(), ["secure", "demo"]);

        ValetCli::unsecure_with_backend(&fake.backend, "demo").unwrap();
        assert_eq!(fake.args(), ["unsecure", "demo"]);
    }

    #[test]
    fn park_maps_command_and_uses_requested_current_dir() {
        let fake = FakeCli::new(SiteBackendKind::Herd);
        let working_dir = fake.config_dir.join("parked projects");
        fs::create_dir_all(&working_dir).unwrap();

        let output = ValetCli::park_with_backend(&fake.backend, &working_dir).unwrap();

        assert_eq!(output, "fake success output");
        assert_eq!(fake.args(), ["park"]);
        assert_eq!(
            fake.cwd(),
            working_dir
                .canonicalize()
                .expect("working dir should exist")
        );
    }

    #[test]
    fn herd_command_failure_names_herd_and_filters_deprecations() {
        let fake = FakeCli::new(SiteBackendKind::Herd);
        fake.fail_with("backend exploded\n");

        let error = ValetCli::secure_with_backend(&fake.backend, "demo")
            .unwrap_err()
            .to_string();

        assert_eq!(error, "Herd secure failed: backend exploded");
    }

    #[test]
    fn valet_command_failure_names_valet() {
        let fake = FakeCli::new(SiteBackendKind::Valet);
        fake.fail_with("valet exploded\n");

        let error = ValetCli::unlink_with_backend(&fake.backend, "demo")
            .unwrap_err()
            .to_string();

        assert_eq!(error, "Valet unlink failed: valet exploded");
    }

    #[test]
    fn spawn_failure_context_names_the_selected_backend() {
        let temp = tempfile::TempDir::new().unwrap();
        let backend = SiteCliBackend {
            kind: SiteBackendKind::Herd,
            executable: temp.path().join("missing Herd CLI"),
        };

        let error = ValetCli::link_with_backend(&backend, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("Failed to run Herd link"));
    }

    #[test]
    fn isolate_remains_valet_only_and_preserves_its_existing_argv() {
        let herd = FakeCli::new(SiteBackendKind::Herd);
        let selected =
            SiteCliBackend::select(BackendPolicy::ValetOnly, true, Some(&herd.config_dir), None)
                .unwrap();
        assert_eq!(selected, SiteCliBackend::valet());

        let valet = FakeCli::new(SiteBackendKind::Valet);
        ValetCli::isolate_with_backend(&valet.backend, "demo", "8.3").unwrap();
        assert_eq!(valet.args(), ["isolate", "demo", "--site=demo", "php@8.3"]);
    }
}
