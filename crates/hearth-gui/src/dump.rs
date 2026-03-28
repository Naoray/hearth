use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;

/// Start a background task that connects to the dump relay and emits events.
///
/// The relay listens on `dump_port + 1` (broadcast port). Each line received
/// is timestamped and emitted as a `dump-line` event to the frontend.
pub fn start_dump_listener(app: &AppHandle, dump_port: u16) {
    let app_handle = app.clone();
    let relay_port = hearth_lib::dump::relay_port(dump_port);

    tauri::async_runtime::spawn(async move {
        loop {
            match TcpStream::connect(format!("127.0.0.1:{relay_port}")).await {
                Ok(stream) => {
                    let _ = app_handle.emit("dump-connected", ());
                    let reader = BufReader::new(stream);
                    let mut lines = reader.lines();

                    loop {
                        match lines.next_line().await {
                            Ok(Some(line)) => {
                                let timestamp =
                                    chrono::Local::now().format("%H:%M:%S").to_string();
                                let formatted = format!("[{timestamp}] {line}");
                                let _ = app_handle.emit("dump-line", formatted);
                            }
                            Ok(None) => {
                                // Connection closed
                                break;
                            }
                            Err(_) => {
                                break;
                            }
                        }
                    }

                    let _ = app_handle.emit("dump-disconnected", ());
                }
                Err(_) => {
                    // Relay not available, will retry
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}
