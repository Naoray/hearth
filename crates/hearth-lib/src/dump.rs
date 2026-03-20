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
pub fn relay_port(dump_port: u16) -> u16 {
    dump_port + 1
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

/// Connect to the dump relay and stream output to stdout.
///
/// Used by `hearth dump` CLI command. Connects to the relay port
/// (dump_port + 1) to receive broadcast dump data.
pub async fn stream_dumps(port: u16) -> anyhow::Result<()> {
    let addr = dump_addr(relay_port(port));
    let stream = TcpStream::connect(addr).await?;
    let (reader, _) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        print!("{}", line);
        line.clear();
    }

    Ok(())
}
