use super::supervisor::ManagedService;
use super::ServiceKind;
use crate::config::HearthConfig;

/// Build the default set of managed services based on configuration.
///
/// Services are configured here but NOT started — the daemon calls
/// `supervisor.start_all()` when ready.
pub fn default_services(config: &HearthConfig) -> Vec<ManagedService> {
    let config_dir = crate::config_dir();

    vec![
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
    ]
}
