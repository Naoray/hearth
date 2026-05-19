//! TCP health probes for database engines.
//!
//! - `tcp_probe` — async, used by `hearth db status` (never on the 5s health loop).
//! - `port_in_use` — sync, used at registration time AND inside Db dispatch
//!   handlers to surface a typed conflict (Herd-Pro-started-after-Hearth race).

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Async TCP probe with a hard timeout. Returns true iff a TCP connection
/// completed within `timeout_ms`.
pub async fn tcp_probe(host: &str, port: u16, timeout_ms: u64) -> bool {
    let addr = format!("{host}:{port}");
    tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Synchronous port-in-use probe with a 200ms connect timeout.
///
/// Used at supervisor-registration time AND inside Db dispatch handlers to
/// surface typed `DaemonResponse::Conflict` instead of letting the circuit
/// breaker burn 3-in-60s on a port collision.
pub fn port_in_use(host: &str, port: u16) -> bool {
    let addr: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tcp_probe_false_on_unbound_port() {
        // Pick a high port unlikely to be bound.
        assert!(!tcp_probe("127.0.0.1", 1, 200).await);
    }

    #[tokio::test]
    async fn tcp_probe_true_on_listening_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Keep listener alive while probing.
        assert!(tcp_probe("127.0.0.1", port, 500).await);
        drop(listener);
    }

    #[test]
    fn port_in_use_returns_true_for_listener() {
        // Bind a std listener on an OS-assigned port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_in_use("127.0.0.1", port));
        drop(listener);
    }

    #[test]
    fn port_in_use_returns_false_for_unbound() {
        // Bind, capture port, drop → port should now be free.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        // Small race: the kernel may keep the port in TIME_WAIT briefly. Probe is
        // a *connect* attempt, not a bind, so it'll fail-fast with ECONNREFUSED.
        assert!(!port_in_use("127.0.0.1", port));
    }
}
