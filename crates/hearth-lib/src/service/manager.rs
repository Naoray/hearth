use super::supervisor::ManagedService;
use super::ServiceKind;
use crate::config::HearthConfig;

/// Build the default set of managed services based on configuration.
///
/// Services are configured here but NOT started — the daemon calls
/// `supervisor.start_all()` when ready.
pub fn default_services(config: &HearthConfig, config_dir: &std::path::Path) -> Vec<ManagedService> {

    let mut services = vec![
        // Nginx — delegates to Valet's installed nginx
        ManagedService::new(
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
        ),
        // dnsmasq on unprivileged port
        ManagedService::new(
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
        ),
        // PHP-FPM using the active PHP version
        ManagedService::new(
            ServiceKind::PhpFpm,
            "php-fpm".to_string(), // resolved at runtime via PhpManager
            vec![
                "--nodaemonize".to_string(),
                format!(
                    "--fpm-config={}",
                    config_dir.join("fpm/php-fpm.conf").display()
                ),
            ],
        ),
    ];

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
    fn default_services_contains_expected_kinds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = HearthConfig::default();
        let services = default_services(&config, tmp.path());

        let kinds: Vec<ServiceKind> = services.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&ServiceKind::Nginx));
        assert!(kinds.contains(&ServiceKind::Dnsmasq));
        assert!(kinds.contains(&ServiceKind::PhpFpm));
        // Mailpit excluded — no binary in temp dir
        assert!(!kinds.contains(&ServiceKind::Mailpit));
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
