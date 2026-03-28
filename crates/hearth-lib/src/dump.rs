use std::net::SocketAddr;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

/// Build a loopback address for the given port.
pub fn dump_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Relay port where CLI subscribers connect (input port + 1).
///
/// Panics in debug builds if dump_port is u16::MAX. In release,
/// saturates to u16::MAX (65535).
pub fn relay_port(dump_port: u16) -> u16 {
    debug_assert!(dump_port < u16::MAX, "dump_port must be less than u16::MAX");
    dump_port.saturating_add(1)
}

/// Run the dump server with broadcast relay.
///
/// - `port` accepts Symfony VarDumper TCP connections (input).
/// - `port + 1` accepts CLI/GUI subscriber connections (output).
///
/// VarDumper data is broadcast to all connected subscribers.
pub async fn run_dump_server(port: u16) -> anyhow::Result<()> {
    let (tx, _) = broadcast::channel::<String>(256);

    let input_addr = dump_addr(port);
    let relay_addr = dump_addr(relay_port(port));

    let input_listener = TcpListener::bind(input_addr).await?;
    let relay_listener = TcpListener::bind(relay_addr).await?;

    info!(port, "dump server listening (VarDumper input)");
    info!(relay_port = relay_port(port), "dump relay listening (subscribers)");

    // Spawn relay acceptor for CLI subscribers
    let relay_tx = tx.clone();
    tokio::spawn(async move {
        loop {
            match relay_listener.accept().await {
                Ok((stream, peer)) => {
                    info!(%peer, "dump subscriber connected");
                    let rx = relay_tx.subscribe();
                    tokio::spawn(async move {
                        if let Err(e) = handle_dump_subscriber(stream, rx).await {
                            error!(error = %e, "dump subscriber error");
                        }
                    });
                }
                Err(e) => error!(error = %e, "dump relay accept error"),
            }
        }
    });

    // Accept VarDumper connections (input)
    loop {
        match input_listener.accept().await {
            Ok((stream, peer)) => {
                info!(%peer, "VarDumper client connected");
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_dump_client(stream, tx).await {
                        error!(error = %e, "dump client error");
                    }
                });
            }
            Err(e) => {
                error!(error = %e, "dump server accept error");
            }
        }
    }
}

/// Handle a VarDumper connection: read dump payloads and broadcast them.
async fn handle_dump_client(
    stream: TcpStream,
    tx: broadcast::Sender<String>,
) -> anyhow::Result<()> {
    let (reader, _) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let trimmed = line.trim().to_string();
        if !trimmed.is_empty() {
            // Broadcast to subscribers; OK if none are connected
            let _ = tx.send(trimmed);
        }
        line.clear();
    }

    Ok(())
}

/// Handle a CLI subscriber connection: relay broadcast data to the client.
async fn handle_dump_subscriber(
    stream: TcpStream,
    mut rx: broadcast::Receiver<String>,
) -> anyhow::Result<()> {
    let (_, mut writer) = stream.into_split();

    loop {
        match rx.recv().await {
            Ok(line) => {
                writer.write_all(line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "dump subscriber lagged, dropped messages");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    Ok(())
}

/// Format a dump line with a dim timestamp prefix.
pub fn format_dump_line(line: &str) -> String {
    let now = chrono::Local::now();
    format!("\x1b[2m[{}]\x1b[0m {}", now.format("%H:%M:%S"), line)
}

/// Connect to the dump relay and stream output to stdout with timestamps.
///
/// Auto-reconnects if the daemon restarts. Used by `hearth dump` CLI command.
/// Runs indefinitely — exits on Ctrl+C or irrecoverable error.
pub async fn stream_dumps(port: u16) -> anyhow::Result<()> {
    let addr = dump_addr(relay_port(port));

    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                eprintln!("Listening for dumps on port {}...", port);
                let (reader, _) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();

                loop {
                    match reader.read_line(&mut line).await {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            print!("{}", format_dump_line(line.trim_end()));
                            println!();
                            line.clear();
                        }
                        Err(e) => {
                            warn!(error = %e, "dump stream read error");
                            break;
                        }
                    }
                }

                eprintln!("Connection lost, reconnecting...");
            }
            Err(e) => {
                warn!(error = %e, "failed to connect to dump relay");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_dump_line_prepends_timestamp() {
        let line = "some dump output";
        let formatted = format_dump_line(line);
        // Should match pattern: \x1b[2m[HH:MM:SS]\x1b[0m some dump output
        assert!(formatted.contains(line));
        assert!(formatted.starts_with("\x1b[2m["));
        assert!(formatted.contains("]\x1b[0m "));
    }

    #[test]
    fn dump_addr_returns_loopback() {
        let addr = dump_addr(9912);
        assert_eq!(addr.ip(), std::net::Ipv4Addr::LOCALHOST);
        assert_eq!(addr.port(), 9912);
    }

    #[test]
    fn relay_port_is_input_plus_one() {
        assert_eq!(relay_port(9912), 9913);
        assert_eq!(relay_port(0), 1);
    }

    #[tokio::test]
    async fn dump_server_broadcasts_to_subscriber() {
        // Bind to OS-assigned ports to avoid conflicts
        let input_listener =
            TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let input_port = input_listener.local_addr().unwrap().port();

        let relay_listener =
            TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let relay_port = relay_listener.local_addr().unwrap().port();

        let (tx, _) = tokio::sync::broadcast::channel::<String>(16);

        // Spawn a subscriber handler
        let sub_tx = tx.clone();
        let relay_handle = tokio::spawn(async move {
            let (stream, _) = relay_listener.accept().await.unwrap();
            let rx = sub_tx.subscribe();
            handle_dump_subscriber(stream, rx).await.unwrap();
        });

        // Spawn a VarDumper client handler
        let client_tx = tx.clone();
        let input_handle = tokio::spawn(async move {
            let (stream, _) = input_listener.accept().await.unwrap();
            handle_dump_client(stream, client_tx).await.unwrap();
        });

        // Connect as subscriber
        let sub_stream = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], relay_port)))
            .await
            .unwrap();
        let (sub_reader, _) = sub_stream.into_split();
        let mut sub_reader = BufReader::new(sub_reader);

        // Connect as VarDumper client and send data
        let client_stream = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], input_port)))
            .await
            .unwrap();
        let (_, mut client_writer) = client_stream.into_split();

        use tokio::io::AsyncWriteExt;
        client_writer.write_all(b"dump payload line 1\n").await.unwrap();
        client_writer.write_all(b"dump payload line 2\n").await.unwrap();
        client_writer.shutdown().await.unwrap();

        // Read from subscriber
        let mut received = String::new();
        sub_reader.read_line(&mut received).await.unwrap();
        assert_eq!(received.trim(), "dump payload line 1");

        received.clear();
        sub_reader.read_line(&mut received).await.unwrap();
        assert_eq!(received.trim(), "dump payload line 2");

        // Cleanup
        input_handle.await.unwrap();
        relay_handle.abort();
    }
}
