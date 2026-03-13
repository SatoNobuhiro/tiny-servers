use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tokio::net::UdpSocket;
use tokio::sync::watch;

use crate::common::{is_within_root, log_access, log_system, SharedLog};

const BLOCK_SIZE: usize = 512;
const MAX_FILE_SIZE: usize = 64 * 1024 * 1024; // 64 MB
const OPCODE_RRQ: u16 = 1;
const OPCODE_WRQ: u16 = 2;
const OPCODE_DATA: u16 = 3;
const OPCODE_ACK: u16 = 4;
const OPCODE_ERROR: u16 = 5;

#[derive(Clone)]
pub struct TftpConfig {
    pub root_dir: PathBuf,
    pub port: u16,
    pub bind_addr: String,
}

pub async fn run(config: TftpConfig, log: SharedLog, mut shutdown_rx: watch::Receiver<bool>) {
    let addr = format!("{}:{}", config.bind_addr, config.port);

    let port = config.port;
    let bind_addr = config.bind_addr.clone();

    let socket = match UdpSocket::bind(&addr).await {
        Ok(s) => {
            log_system(&log, "TFTP", &bind_addr, port,
                format!("Server started (folder: {})", config.root_dir.display()));
            s
        }
        Err(e) => {
            log_system(&log, "TFTP", &bind_addr, port, format!("Failed to bind: {}", e));
            return;
        }
    };

    let mut buf = [0u8; 516];

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, addr)) => {
                        let config = config.clone();
                        let log = log.clone();
                        let data = buf[..len].to_vec();
                        tokio::spawn(async move {
                            handle_request(&data, addr, &config, &log).await;
                        });
                    }
                    Err(e) => {
                        log_system(&log, "TFTP", &bind_addr, port, format!("Recv error: {}", e));
                    }
                }
            }
        }
    }

    log_system(&log, "TFTP", &bind_addr, port, "Server listener closed");
}

async fn handle_request(data: &[u8], addr: SocketAddr, config: &TftpConfig, log: &SharedLog) {
    if data.len() < 4 {
        return;
    }
    let opcode = u16::from_be_bytes([data[0], data[1]]);
    match opcode {
        OPCODE_RRQ => handle_rrq(data, addr, config, log).await,
        OPCODE_WRQ => handle_wrq(data, addr, config, log).await,
        _ => {
            log_access(log, "TFTP", &config.bind_addr, config.port, addr, format!("Unknown opcode: {}", opcode));
        }
    }
}

fn parse_request(data: &[u8]) -> Option<(String, String)> {
    let rest = &data[2..];
    let null1 = rest.iter().position(|&b| b == 0)?;
    let filename = String::from_utf8_lossy(&rest[..null1]).to_string();
    let mode_start = null1 + 1;
    if mode_start >= rest.len() {
        return None;
    }
    let null2 = rest[mode_start..].iter().position(|&b| b == 0)?;
    let mode = String::from_utf8_lossy(&rest[mode_start..mode_start + null2]).to_string();
    Some((filename, mode))
}

fn resolve_path(root: &Path, filename: &str) -> Option<PathBuf> {
    let normalized = filename.replace('\\', "/");
    let clean = normalized.trim_start_matches('/');
    for component in Path::new(clean).components() {
        if matches!(component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        ) {
            return None;
        }
    }
    let result = root.join(clean);

    // Final containment check (resolves symlinks/junctions)
    if !is_within_root(root, &result) {
        return None;
    }

    Some(result)
}

async fn handle_rrq(data: &[u8], client: SocketAddr, config: &TftpConfig, log: &SharedLog) {
    let (filename, _mode) = match parse_request(data) {
        Some(r) => r,
        None => return,
    };

    log_access(log, "TFTP", &config.bind_addr, config.port, client, format!("RRQ {}", filename));

    let path = match resolve_path(&config.root_dir, &filename) {
        Some(p) if p.is_file() => p,
        _ => {
            send_error(client, 1, "File not found").await;
            return;
        }
    };

    let file_data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(_) => {
            send_error(client, 1, "File not found").await;
            return;
        }
    };

    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut block: u16 = 1;
    let mut offset = 0;

    loop {
        let end = std::cmp::min(offset + BLOCK_SIZE, file_data.len());
        let chunk = &file_data[offset..end];

        let mut pkt = Vec::with_capacity(4 + chunk.len());
        pkt.extend_from_slice(&OPCODE_DATA.to_be_bytes());
        pkt.extend_from_slice(&block.to_be_bytes());
        pkt.extend_from_slice(chunk);

        let mut retries = 0;
        loop {
            if socket.send_to(&pkt, client).await.is_err() {
                return;
            }
            let mut ack_buf = [0u8; 4];
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                socket.recv_from(&mut ack_buf),
            )
            .await
            {
                Ok(Ok((4, _))) => {
                    let ack_op = u16::from_be_bytes([ack_buf[0], ack_buf[1]]);
                    let ack_blk = u16::from_be_bytes([ack_buf[2], ack_buf[3]]);
                    if ack_op == OPCODE_ACK && ack_blk == block {
                        break;
                    }
                }
                _ => {
                    retries += 1;
                    if retries >= 3 {
                        log_access(log, "TFTP", &config.bind_addr, config.port, client, "Transfer timeout");
                        return;
                    }
                }
            }
        }

        if chunk.len() < BLOCK_SIZE {
            log_access(log, "TFTP", &config.bind_addr, config.port, client,
                format!("Sent {} ({} bytes)", filename, file_data.len()));
            break;
        }

        block = block.wrapping_add(1);
        offset = end;
    }
}

async fn handle_wrq(data: &[u8], client: SocketAddr, config: &TftpConfig, log: &SharedLog) {
    let (filename, _mode) = match parse_request(data) {
        Some(r) => r,
        None => return,
    };

    log_access(log, "TFTP", &config.bind_addr, config.port, client, format!("WRQ {}", filename));

    let path = match resolve_path(&config.root_dir, &filename) {
        Some(p) => p,
        None => {
            send_error(client, 2, "Access violation").await;
            return;
        }
    };

    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return,
    };

    // ACK block 0 to accept
    let _ = socket.send_to(&build_ack(0), client).await;

    let mut file_data = Vec::new();
    let mut expected: u16 = 1;

    loop {
        let mut buf = [0u8; 516];
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.recv_from(&mut buf),
        )
        .await
        {
            Ok(Ok((len, _))) if len >= 4 => {
                let opcode = u16::from_be_bytes([buf[0], buf[1]]);
                let blk = u16::from_be_bytes([buf[2], buf[3]]);
                if opcode != OPCODE_DATA || blk != expected {
                    continue;
                }
                file_data.extend_from_slice(&buf[4..len]);

                if file_data.len() > MAX_FILE_SIZE {
                    send_error_on(
                        &socket, client, 3,
                        &format!("File too large (max {} MB)", MAX_FILE_SIZE / 1024 / 1024),
                    ).await;
                    log_access(log, "TFTP", &config.bind_addr, config.port, client,
                        format!("WRQ {} rejected: exceeds size limit", filename));
                    return;
                }

                let _ = socket.send_to(&build_ack(blk), client).await;

                if len - 4 < BLOCK_SIZE {
                    break;
                }
                expected = expected.wrapping_add(1);
            }
            _ => {
                log_access(log, "TFTP", &config.bind_addr, config.port, client, "Write timeout");
                return;
            }
        }
    }

    match std::fs::write(&path, &file_data) {
        Ok(_) => log_access(log, "TFTP", &config.bind_addr, config.port, client,
            format!("Received {} ({} bytes)", filename, file_data.len())),
        Err(e) => log_access(log, "TFTP", &config.bind_addr, config.port, client,
            format!("Write error: {}", e)),
    }
}

fn build_ack(block: u16) -> [u8; 4] {
    let mut pkt = [0u8; 4];
    pkt[0..2].copy_from_slice(&OPCODE_ACK.to_be_bytes());
    pkt[2..4].copy_from_slice(&block.to_be_bytes());
    pkt
}

async fn send_error_on(socket: &UdpSocket, addr: SocketAddr, code: u16, msg: &str) {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&OPCODE_ERROR.to_be_bytes());
    pkt.extend_from_slice(&code.to_be_bytes());
    pkt.extend_from_slice(msg.as_bytes());
    pkt.push(0);
    let _ = socket.send_to(&pkt, addr).await;
}

async fn send_error(addr: SocketAddr, code: u16, msg: &str) {
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&OPCODE_ERROR.to_be_bytes());
    pkt.extend_from_slice(&code.to_be_bytes());
    pkt.extend_from_slice(msg.as_bytes());
    pkt.push(0);
    let _ = socket.send_to(&pkt, addr).await;
}
