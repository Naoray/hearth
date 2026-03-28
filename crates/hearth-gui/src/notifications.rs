use std::collections::HashMap;
use std::time::Instant;

use tauri::AppHandle;

/// Debounce interval to prevent notification spam.
const COOLDOWN_SECS: u64 = 30;

/// Tracks per-service notification timestamps to prevent spam.
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
    pub fn notify_if_allowed(
        &mut self,
        app: &AppHandle,
        service: &str,
        title: &str,
        body: &str,
    ) {
        let now = Instant::now();

        if let Some(last) = self.last_sent.get(service) {
            if now.duration_since(*last).as_secs() < COOLDOWN_SECS {
                return;
            }
        }

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
