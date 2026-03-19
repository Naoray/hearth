use std::process::Command;

use anyhow::Context;
use tracing::{info, warn};

/// Wrapper around the Valet CLI.
///
/// Hearth delegates site management operations to Valet, which handles
/// Nginx config generation, driver system, SSL certificates, and dnsmasq.
/// This module wraps those CLI calls and parses their output.
///
/// In a future phase, Valet can be replaced with native Rust implementations
/// (strangler fig pattern).
pub struct ValetCli;

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
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Valet installation failed: {}", stderr);
        }

        // Run valet install to set up Nginx, dnsmasq, etc.
        let output = Command::new("valet")
            .arg("install")
            .output()
            .context("Failed to run valet install")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("valet install had issues: {}", stderr);
        }

        Ok(())
    }

    /// Link a directory as a Valet site.
    pub fn link(name: Option<&str>) -> anyhow::Result<String> {
        let mut cmd = Command::new("valet");
        cmd.arg("link");
        if let Some(n) = name {
            cmd.arg(n);
        }

        let output = cmd.output().context("Failed to run valet link")?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("valet link failed: {}", stderr);
        }

        Ok(stdout.trim().to_string())
    }

    /// Unlink a site.
    pub fn unlink(name: &str) -> anyhow::Result<()> {
        let output = Command::new("valet")
            .args(["unlink", name])
            .output()
            .context("Failed to run valet unlink")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("valet unlink failed: {}", stderr);
        }

        Ok(())
    }

    /// Park the current directory.
    pub fn park() -> anyhow::Result<String> {
        let output = Command::new("valet")
            .arg("park")
            .output()
            .context("Failed to run valet park")?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("valet park failed: {}", stderr);
        }

        Ok(stdout.trim().to_string())
    }

    /// Secure a site with SSL.
    pub fn secure(name: &str) -> anyhow::Result<()> {
        let output = Command::new("valet")
            .args(["secure", name])
            .output()
            .context("Failed to run valet secure")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("valet secure failed: {}", stderr);
        }

        Ok(())
    }

    /// Unsecure a site.
    pub fn unsecure(name: &str) -> anyhow::Result<()> {
        let output = Command::new("valet")
            .args(["unsecure", name])
            .output()
            .context("Failed to run valet unsecure")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("valet unsecure failed: {}", stderr);
        }

        Ok(())
    }

    /// Isolate a site to a specific PHP version.
    pub fn isolate(name: &str, php_version: &str) -> anyhow::Result<()> {
        let output = Command::new("valet")
            .args(["isolate", name, &format!("--site={}", name)])
            .arg(format!("php@{}", php_version))
            .output()
            .context("Failed to run valet isolate")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("valet isolate failed: {}", stderr);
        }

        Ok(())
    }
}
