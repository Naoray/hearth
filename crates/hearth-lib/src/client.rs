use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::socket::{DaemonRequest, DaemonResponse};

/// Async client for communicating with the hearth daemon over Unix socket.
///
/// Used by both CLI and GUI to send requests to the daemon.
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Create a client using the default socket path.
    pub fn new() -> Self {
        Self {
            socket_path: crate::socket::socket_path(),
        }
    }

    /// Create a client with a custom socket path.
    pub fn with_socket_path(path: PathBuf) -> Self {
        Self { socket_path: path }
    }

    /// Get the socket path this client connects to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Send a request to the daemon and return the response.
    pub async fn send(&self, request: DaemonRequest) -> anyhow::Result<DaemonResponse> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .context("daemon not running — start with: hearth daemon start")?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let request_json = serde_json::to_string(&request)?;
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut line = String::new();
        reader.read_line(&mut line).await?;

        let response: DaemonResponse = serde_json::from_str(line.trim())?;
        Ok(response)
    }

    /// Check if the daemon is reachable by sending a Ping.
    pub async fn is_daemon_running(&self) -> bool {
        matches!(
            self.send(DaemonRequest::Ping).await,
            Ok(DaemonResponse::Pong)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_uses_default_socket_path() {
        let client = DaemonClient::new();
        assert_eq!(client.socket_path(), crate::socket::socket_path());
    }

    #[test]
    fn client_custom_socket_path() {
        let path = std::path::PathBuf::from("/tmp/test.sock");
        let client = DaemonClient::with_socket_path(path.clone());
        assert_eq!(client.socket_path(), &path);
    }
}
