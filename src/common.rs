use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Local;

#[derive(Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub kind: &'static str,
    pub server: &'static str,
    pub local_ip: String,
    pub local_port: u16,
    pub remote_ip: String,
    pub remote_port: String,
    pub message: String,
}

pub type SharedLog = Arc<Mutex<Vec<LogEntry>>>;

pub fn log_system(log: &SharedLog, server: &'static str, bind_addr: &str, port: u16, msg: impl Into<String>) {
    let entry = LogEntry {
        timestamp: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        kind: "System",
        server,
        local_ip: bind_addr.to_string(),
        local_port: port,
        remote_ip: "-".to_string(),
        remote_port: "-".to_string(),
        message: format!("{} {}", server, msg.into()),
    };
    if let Ok(mut logs) = log.lock() {
        logs.push(entry);
    }
}

pub fn log_access(
    log: &SharedLog,
    server: &'static str,
    bind_addr: &str,
    port: u16,
    remote: SocketAddr,
    msg: impl Into<String>,
) {
    let entry = LogEntry {
        timestamp: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        kind: "Access",
        server,
        local_ip: bind_addr.to_string(),
        local_port: port,
        remote_ip: remote.ip().to_string(),
        remote_port: remote.port().to_string(),
        message: msg.into(),
    };
    if let Ok(mut logs) = log.lock() {
        logs.push(entry);
    }
}

/// Verify that `path` is contained within `root` after resolving symlinks.
/// For non-existing paths (e.g. upload targets), checks the parent directory.
pub fn is_within_root(root: &Path, path: &Path) -> bool {
    let canon_root = match root.canonicalize() {
        Ok(r) => r,
        Err(_) => return false,
    };

    if path.exists() {
        match path.canonicalize() {
            Ok(canon) => canon.starts_with(&canon_root),
            Err(_) => false,
        }
    } else {
        // For paths that don't exist yet (STOR, MKD, WRQ), verify the parent
        match path.parent() {
            Some(parent) if parent.exists() => {
                match parent.canonicalize() {
                    Ok(canon_parent) => canon_parent.starts_with(&canon_root),
                    Err(_) => false,
                }
            }
            _ => false,
        }
    }
}
