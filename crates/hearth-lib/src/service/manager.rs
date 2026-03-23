use tracing::info;

use super::supervisor::ManagedService;
use super::ServiceKind;
use crate::config::HearthConfig;

/// Detect whether Laravel Herd is running by checking for its process.
pub fn is_herd_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-q", "-f", "Herd\\.app"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Build the default set of managed services based on configuration.
///
/// When Herd is detected, nginx/php-fpm/dnsmasq are skipped — Herd
/// manages those. Hearth only registers its own services (Mailpit, etc.).
/// The dump server and MCP server are Tokio tasks, not supervised processes.
pub fn default_services(config: &HearthConfig, config_dir: &std::path::Path) -> Vec<ManagedService> {
    let mut services = Vec::new();

    if is_herd_running() {
        info!("Herd detected — skipping nginx, php-fpm, dnsmasq (managed by Herd)");
    } else {
        services.push(ManagedService::new(
            ServiceKind::Nginx,
            "nginx".to_string(),
            vec![
                "-c".to_string(),
                config_dir
                    .join("nginx/nginx.conf")
                    .to_string_lossy()
                    .to_string(),
                "-g".to_string(),
                "daemon off;".to_string(),
            ],
        ));
        services.push(ManagedService::new(
            ServiceKind::Dnsmasq,
            "dnsmasq".to_string(),
            vec![
                "--keep-in-foreground".to_string(),
                format!("--port={}", config.dns_port),
                format!(
                    "--conf-file={}",
                    config_dir.join("dnsmasq/dnsmasq.conf").display()
                ),
            ],
        ));
        services.push(ManagedService::new(
            ServiceKind::PhpFpm,
            "php-fpm".to_string(),
            vec![
                "--nodaemonize".to_string(),
                format!(
                    "--fpm-config={}",
                    config_dir.join("fpm/php-fpm.conf").display()
                ),
            ],
        ));
    }

    // Mailpit — only if binary is found
    if let Some(mailpit_bin) = crate::mailpit::resolve_mailpit_binary(config_dir) {
        services.push(ManagedService::new(
            ServiceKind::Mailpit,
            mailpit_bin.to_string_lossy().to_string(),
            vec![
                "--smtp".to_string(),
                format!("127.0.0.1:{}", config.mail_smtp_port),
                "--listen".to_string(),
                format!("127.0.0.1:{}", config.mail_ui_port),
                "--db-file".to_string(),
                config_dir
                    .join("services/mailpit/mailpit.db")
                    .to_string_lossy()
                    .to_string(),
            ],
        ));
    }

    services
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HearthConfig;

    #[test]
    fn herd_detection_returns_bool() {
        // Just verify it doesn't panic — result depends on whether Herd is running
        let _ = is_herd_running();
    }

    #[test]
    fn default_services_includes_mailpit_when_binary_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_path = tmp.path().join("services/mailpit/mailpit");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "fake").unwrap();

        let config = HearthConfig::default();
        let services = default_services(&config, tmp.path());

        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&ServiceKind::Mailpit));
    }

    #[test]
    fn default_services_excludes_mailpit_when_binary_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = HearthConfig::default();
        let services = default_services(&config, tmp.path());

        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(!kinds.contains(&ServiceKind::Mailpit));
    }
}
