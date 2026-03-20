use std::net::SocketAddr;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info};

/// Default dump server address.
pub fn dump_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Run the dump server relay.
///
/// Listens for Symfony VarDumper TCP connections on the given port.
/// Each dump payload is a single line of serialized data sent by
/// `symfony/var-dumper`'s `ServerDumper` client.
///
/// This function runs forever until cancelled.
pub async fn run_dump_server(port: u16) -> anyhow::Result<()> {
    let addr = dump_addr(port);
    let listener = TcpListener::bind(addr).await?;
    info!(port, "dump server listening");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!(%peer, "dump client connected");
                tokio::spawn(async move {
                    if let Err(e) = handle_dump_client(stream).await {
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

/// Handle a single dump client connection.
///
/// Reads line-delimited dump payloads and writes them to stdout
/// with ANSI formatting. In the future, this can be extended to
/// broadcast to connected CLI/GUI clients via a channel.
async fn handle_dump_client(stream: TcpStream) -> anyhow::Result<()> {
    let (reader, _writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        // VarDumper sends serialized data; for now, relay raw content.
        // A future enhancement can parse and pretty-print the dump data.
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            // Write to stdout so CLI clients piping from this process see output
            let mut stdout = tokio::io::stdout();
            stdout.write_all(trimmed.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
        line.clear();
    }

    Ok(())
}

/// Connect to the dump server as a client and stream output to stdout.
///
/// Used by `hearth dump` CLI command. Connects directly to the dump
/// server's TCP port rather than going through the daemon socket.
pub async fn stream_dumps(port: u16) -> anyhow::Result<()> {
    let addr = dump_addr(port);
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
