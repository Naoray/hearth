use std::collections::HashMap;
use std::time::{Duration, Instant};

use tauri::AppHandle;

/// Debounce interval to prevent notification spam (30 seconds per service).
const COOLDOWN: Duration = Duration::from_secs(30);

/// Tracks per-service notification timestamps to prevent spam.
///
/// Each service has an independent cooldown — a failure notification for
/// "nginx" does not suppress one for "php-fpm".
pub struct NotificationDebouncer {
    last_sent: HashMap<String, Instant>,
}

impl NotificationDebouncer {
    pub fn new() -> Self {
        Self {
            last_sent: HashMap::new(),
        }
    }

    /// Send a notification if the cooldown for this service has elapsed.
    ///
    /// Skips silently when the last notification for the same service key
    /// was sent less than 30 seconds ago.
    pub fn notify_if_allowed(
        &mut self,
        app: &AppHandle,
        service: &str,
        title: &str,
        body: &str,
    ) {
        let now = Instant::now();

        if let Some(last) = self.last_sent.get(service) {
            if now.duration_since(*last) < COOLDOWN {
                tracing::debug!(service, "notification suppressed (cooldown active)");
                return;
            }
        }

        tracing::info!(service, title, body, "sending macOS notification");

        #[cfg(not(test))]
        {
            use tauri_plugin_notification::NotificationExt;
            let _ = app.notification().builder().title(title).body(body).show();
        }

        #[cfg(test)]
        {
            let _ = (app, title, body);
        }

        self.last_sent.insert(service.to_string(), now);
    }
}
