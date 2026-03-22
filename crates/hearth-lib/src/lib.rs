pub mod config;
pub mod download;
pub mod dump;
pub mod mailpit;
pub mod php;
pub mod service;
pub mod site;
pub mod socket;
pub mod valet;

use std::path::PathBuf;

/// Default configuration directory: ~/.config/hearth/
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .expect("could not determine config directory")
        .join("hearth")
}

/// Default runtime directory for PID files and socket
pub fn run_dir() -> PathBuf {
    config_dir().join("run")
}

/// Default log directory
pub fn log_dir() -> PathBuf {
    config_dir().join("log")
}
