//! Declarative prompt definitions consumed by the CLI.
//!
//! Recipes return `Vec<Prompt>` to describe what the CLI should ask. The CLI renders
//! them with `dialoguer`, validates per-`Prompt`, and ships the collected answers to
//! the daemon as `AddAnswers`. Daemon never opens a TTY.

/// Optional post-input validator for `Prompt::Integer`. The CLI runs the named
/// validator before accepting a value. Enum (not closure) so prompts stay
/// declarative and trivially testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatorKind {
    /// No extra validation beyond the integer range.
    None,
    /// Probe `127.0.0.1:<value>` with `TcpListener::bind`. Reject if busy.
    PortFree,
}

#[derive(Debug, Clone)]
pub enum Prompt {
    Text {
        key: String,
        message: String,
        default: Option<String>,
    },
    Confirm {
        key: String,
        message: String,
        default: bool,
    },
    Select {
        key: String,
        message: String,
        options: Vec<String>,
        default_index: usize,
    },
    Integer {
        key: String,
        message: String,
        default: i64,
        min: i64,
        max: i64,
        validator: ValidatorKind,
    },
}

impl Prompt {
    pub fn key(&self) -> &str {
        match self {
            Prompt::Text { key, .. }
            | Prompt::Confirm { key, .. }
            | Prompt::Select { key, .. }
            | Prompt::Integer { key, .. } => key,
        }
    }
}

/// Probe whether a TCP port is free on 127.0.0.1.
pub fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// First free port in [start..=end] inclusive, capped at `max_tries` to avoid runaway.
/// Returns `None` if every probed port is busy.
pub fn first_free_port(start: u16, end: u16, max_tries: u16) -> Option<u16> {
    let upper = end.min(start.saturating_add(max_tries.saturating_sub(1)));
    (start..=upper).find(|p| port_is_free(*p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator_kind_eq() {
        assert_eq!(ValidatorKind::None, ValidatorKind::None);
        assert_ne!(ValidatorKind::None, ValidatorKind::PortFree);
    }

    #[test]
    fn prompt_key_extracts_key() {
        let p = Prompt::Text {
            key: "horizon_environment".to_string(),
            message: "Env?".to_string(),
            default: None,
        };
        assert_eq!(p.key(), "horizon_environment");

        let p = Prompt::Integer {
            key: "reverb_port".to_string(),
            message: "Port?".to_string(),
            default: 8080,
            min: 1024,
            max: 65535,
            validator: ValidatorKind::PortFree,
        };
        assert_eq!(p.key(), "reverb_port");
    }

    #[test]
    fn port_is_free_detects_bound_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Port is currently held by `listener` — should report busy.
        assert!(!port_is_free(port));
        drop(listener);
        // After drop, OS may keep TIME_WAIT briefly; we don't re-assert true to avoid flakes.
    }

    #[test]
    fn first_free_port_caps_max_tries() {
        // Hold ports start..start+2 to exhaust a small range.
        let l1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p1 = l1.local_addr().unwrap().port();
        // Asking for a range that includes p1 with max_tries=1 from p1 should return None.
        assert_eq!(first_free_port(p1, p1.saturating_add(50), 1), None);
        drop(l1);
    }
}
