use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use chrono::{DateTime, Datelike, Local};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const MAX_CMD_LINE: usize = 4096;

use crate::common::{is_within_root, log_access, log_system, SharedLog};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ServerConfig {
    pub root_dir: PathBuf,
    pub port: u16,
    pub bind_addr: SocketAddr,
    pub username: Option<String>,
    pub password: Option<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub async fn run(config: ServerConfig, log: SharedLog, mut shutdown_rx: watch::Receiver<bool>) {
    let addr = config.bind_addr;
    let port = config.port;
    let bind_ip = addr.ip().to_string();

    let listener = match TcpListener::bind(addr).await {
        Ok(l) => {
            log_system(&log, "FTP", &bind_ip, port,
                format!("Server started (folder: {})", config.root_dir.display()));
            l
        }
        Err(e) => {
            log_system(&log, "FTP", &bind_ip, port, format!("Failed to bind: {}", e));
            return;
        }
    };

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, peer_addr)) => {
                        log_access(&log, "FTP", &bind_ip, port, peer_addr, "Connected");
                        let session = FtpSession::new(
                            stream,
                            peer_addr,
                            config.clone(),
                            log.clone(),
                        );
                        tokio::spawn(async move {
                            if let Err(e) = session.run().await {
                                let _ = e;
                            }
                        });
                    }
                    Err(e) => {
                        log_system(&log, "FTP", &bind_ip, port, format!("Accept error: {}", e));
                    }
                }
            }
        }
    }

    log_system(&log, "FTP", &bind_ip, port, "Server listener closed");
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct FtpSession {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    root_dir: PathBuf,
    current_dir: String,
    data_listener: Option<TcpListener>,
    transfer_type: char,
    authenticated: bool,
    got_user: bool,
    expected_user: Option<String>,
    expected_pass: Option<String>,
    log: SharedLog,
    port: u16,
    peer_addr: SocketAddr,
    local_ip: IpAddr,
    bind_addr_str: String,
}

impl FtpSession {
    fn new(
        stream: TcpStream,
        peer_addr: SocketAddr,
        config: ServerConfig,
        log: SharedLog,
    ) -> Self {
        let bind_addr_str = config.bind_addr.ip().to_string();
        let local_ip = stream.local_addr().map(|a| a.ip()).unwrap_or(IpAddr::from([127, 0, 0, 1]));
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();

        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            root_dir: config.root_dir,
            current_dir: "/".to_string(),
            data_listener: None,
            transfer_type: 'A',
            authenticated: config.username.is_none(),
            got_user: false,
            expected_user: config.username,
            expected_pass: config.password,
            log,
            port: config.port,
            peer_addr,
            local_ip,
            bind_addr_str,
        }
    }

    fn log(&self, msg: impl Into<String>) {
        log_access(&self.log, "FTP", &self.bind_addr_str, self.port, self.peer_addr, msg);
    }

    async fn run(mut self) -> io::Result<()> {
        self.send("220 Simple FTP Server ready\r\n").await?;

        loop {
            match self.read_command().await {
                Ok(Some(cmd_line)) => {
                    if let Err(e) = self.handle_command(&cmd_line).await {
                        self.log(format!("Error: {}", e));
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    self.send("500 Command line too long\r\n").await?;
                }
                Err(e) => {
                    self.log(format!("Read error: {}", e));
                    break;
                }
            }
        }

        self.log("Disconnected");
        Ok(())
    }

    /// Read one command line, bounded to MAX_CMD_LINE bytes.
    async fn read_command(&mut self) -> io::Result<Option<String>> {
        let mut buf = Vec::with_capacity(256);
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(bytes_to_string(&buf).trim_end().to_string()))
                };
            }

            if let Some(pos) = available.iter().position(|&b| b == b'\n') {
                buf.extend_from_slice(&available[..pos]);
                self.reader.consume(pos + 1);
                return Ok(Some(bytes_to_string(&buf).trim_end().to_string()));
            }

            let len = available.len();
            buf.extend_from_slice(available);
            self.reader.consume(len);

            if buf.len() > MAX_CMD_LINE {
                // Drain the rest of the oversized line
                loop {
                    let avail = self.reader.fill_buf().await?;
                    if avail.is_empty() { break; }
                    let pos = avail.iter().position(|&b| b == b'\n');
                    let n = pos.map(|p| p + 1).unwrap_or(avail.len());
                    self.reader.consume(n);
                    if pos.is_some() { break; }
                }
                return Err(io::Error::new(io::ErrorKind::InvalidData, "too long"));
            }
        }
    }

    async fn send(&mut self, msg: &str) -> io::Result<()> {
        self.writer.write_all(msg.as_bytes()).await
    }

    async fn handle_command(&mut self, cmd_line: &str) -> io::Result<()> {
        let (cmd, args) = match cmd_line.find(' ') {
            Some(i) => (cmd_line[..i].to_uppercase(), cmd_line[i + 1..].to_string()),
            None => (cmd_line.to_uppercase(), String::new()),
        };

        // Log command (mask password)
        let display = if cmd == "PASS" {
            format!("PASS ****")
        } else {
            cmd_line.to_string()
        };
        self.log(display);

        match cmd.as_str() {
            "USER" => self.cmd_user(&args).await,
            "PASS" => self.cmd_pass(&args).await,
            "SYST" => self.send("215 Windows_NT\r\n").await,
            "FEAT" => {
                self.send("211-Features:\r\n PASV\r\n SIZE\r\n UTF8\r\n211 End\r\n").await
            }
            "OPTS" => self.cmd_opts(&args).await,
            "PWD" | "XPWD" => self.cmd_pwd().await,
            "CWD" | "XCWD" => self.cmd_cwd(&args).await,
            "CDUP" | "XCUP" => self.cmd_cwd("..").await,
            "TYPE" => self.cmd_type(&args).await,
            "PASV" => self.cmd_pasv().await,
            "EPSV" => self.cmd_epsv().await,
            "LIST" => self.cmd_list(&args).await,
            "NLST" => self.cmd_nlst(&args).await,
            "RETR" => self.cmd_retr(&args).await,
            "STOR" => self.cmd_stor(&args).await,
            "SIZE" => self.cmd_size(&args).await,
            "DELE" => self.cmd_dele(&args).await,
            "MKD" | "XMKD" => self.cmd_mkd(&args).await,
            "RMD" | "XRMD" => self.cmd_rmd(&args).await,
            "RNFR" => self.cmd_rnfr(&args).await,
            "RNTO" => self.cmd_rnto(&args).await,
            "NOOP" => self.send("200 OK\r\n").await,
            "QUIT" => {
                self.send("221 Goodbye\r\n").await?;
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "QUIT"));
            }
            _ => {
                self.send(&format!("502 Command not implemented: {}\r\n", cmd))
                    .await
            }
        }
    }

    // --- Authentication ---

    async fn cmd_user(&mut self, args: &str) -> io::Result<()> {
        let user = args.trim();
        self.got_user = true;

        match &self.expected_user {
            None => {
                self.authenticated = true;
                self.send("230 User logged in\r\n").await
            }
            Some(expected) => {
                if user == expected.as_str() {
                    if self.expected_pass.is_none() {
                        self.authenticated = true;
                        self.send("230 User logged in\r\n").await
                    } else {
                        self.send("331 Password required\r\n").await
                    }
                } else {
                    self.send("530 Invalid username\r\n").await
                }
            }
        }
    }

    async fn cmd_pass(&mut self, args: &str) -> io::Result<()> {
        if !self.got_user {
            return self.send("503 Login with USER first\r\n").await;
        }

        match &self.expected_pass {
            None => {
                self.authenticated = true;
                self.send("230 User logged in\r\n").await
            }
            Some(expected) => {
                if args.trim() == expected.as_str() {
                    self.authenticated = true;
                    self.send("230 User logged in\r\n").await
                } else {
                    self.send("530 Login incorrect\r\n").await
                }
            }
        }
    }

    fn require_auth(&self) -> bool {
        self.authenticated
    }

    // --- Options ---

    async fn cmd_opts(&mut self, args: &str) -> io::Result<()> {
        if args.to_uppercase().starts_with("UTF8") {
            self.send("200 UTF8 mode enabled\r\n").await
        } else {
            self.send("501 Option not understood\r\n").await
        }
    }

    // --- Navigation ---

    fn normalize_ftp_path(&self, path: &str) -> String {
        // Normalize backslashes to forward slashes to prevent bypass
        let path = path.replace('\\', "/");
        let raw = if path.starts_with('/') {
            path.to_string()
        } else {
            format!(
                "{}/{}",
                self.current_dir.trim_end_matches('/'),
                path
            )
        };

        let mut parts: Vec<&str> = Vec::new();
        for part in raw.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        if parts.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", parts.join("/"))
        }
    }

    fn resolve_path(&self, ftp_path: &str) -> Option<PathBuf> {
        let normalized = self.normalize_ftp_path(ftp_path);
        let rel = normalized.trim_start_matches('/');

        // Reject Windows absolute paths (drive letters, UNC, etc.)
        for component in std::path::Path::new(rel).components() {
            if matches!(component,
                std::path::Component::Prefix(_) | std::path::Component::ParentDir
            ) {
                return None;
            }
        }

        let result = if rel.is_empty() {
            self.root_dir.clone()
        } else {
            self.root_dir.join(rel)
        };

        // Final containment check (resolves symlinks/junctions)
        if !is_within_root(&self.root_dir, &result) {
            return None;
        }

        Some(result)
    }

    async fn cmd_pwd(&mut self) -> io::Result<()> {
        self.send(&format!("257 \"{}\"\r\n", self.current_dir)).await
    }

    async fn cmd_cwd(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let target = self.normalize_ftp_path(args.trim());
        let real = match self.resolve_path(args.trim()) {
            Some(p) => p,
            None => return self.send("550 Access denied\r\n").await,
        };

        if real.is_dir() {
            self.current_dir = target;
            self.send(&format!("250 Directory changed to {}\r\n", self.current_dir))
                .await
        } else {
            self.send("550 Directory not found\r\n").await
        }
    }

    // --- Transfer type ---

    async fn cmd_type(&mut self, args: &str) -> io::Result<()> {
        let t = args.trim().chars().next().unwrap_or('A').to_ascii_uppercase();
        match t {
            'A' | 'I' => {
                self.transfer_type = t;
                self.send(&format!("200 Type set to {}\r\n", t)).await
            }
            _ => self.send("504 Type not supported\r\n").await,
        }
    }

    // --- Data connection (PASV / EPSV) ---

    async fn cmd_pasv(&mut self) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let ip = match self.local_ip {
            IpAddr::V4(v4) => v4,
            _ => std::net::Ipv4Addr::new(127, 0, 0, 1),
        };

        let listener = TcpListener::bind((ip, 0u16)).await?;
        let port = listener.local_addr()?.port();
        let octets = ip.octets();
        let p1 = port / 256;
        let p2 = port % 256;

        self.data_listener = Some(listener);

        self.send(&format!(
            "227 Entering Passive Mode ({},{},{},{},{},{})\r\n",
            octets[0], octets[1], octets[2], octets[3], p1, p2
        ))
        .await
    }

    async fn cmd_epsv(&mut self) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let listener = TcpListener::bind(("0.0.0.0", 0u16)).await?;
        let port = listener.local_addr()?.port();
        self.data_listener = Some(listener);

        self.send(&format!("229 Entering Extended Passive Mode (|||{}|)\r\n", port))
            .await
    }

    async fn accept_data(&mut self) -> io::Result<TcpStream> {
        match self.data_listener.take() {
            Some(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok(stream)
            }
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "No data connection (use PASV first)",
            )),
        }
    }

    // --- LIST / NLST ---

    async fn cmd_list(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path_arg = strip_ls_flags(args.trim());
        let target = if path_arg.is_empty() {
            match self.resolve_path(&self.current_dir.clone()) {
                Some(p) => p,
                None => return self.send("550 Access denied\r\n").await,
            }
        } else {
            match self.resolve_path(path_arg) {
                Some(p) => p,
                None => return self.send("550 Access denied\r\n").await,
            }
        };

        if !target.is_dir() {
            return self.send("550 Directory not found\r\n").await;
        }

        self.send("150 Opening data connection for directory listing\r\n")
            .await?;

        let mut data = match self.accept_data().await {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send(&format!("425 Cannot open data connection: {}\r\n", e))
                    .await;
            }
        };

        let mut listing = String::new();
        match std::fs::read_dir(&target) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        listing.push_str(&format_list_entry(&entry, &meta));
                        listing.push_str("\r\n");
                    }
                }
            }
            Err(e) => {
                let _ = data.shutdown().await;
                return self
                    .send(&format!("550 Cannot read directory: {}\r\n", e))
                    .await;
            }
        }

        let _ = data.write_all(listing.as_bytes()).await;
        let _ = data.shutdown().await;

        self.send("226 Transfer complete\r\n").await
    }

    async fn cmd_nlst(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path_arg = strip_ls_flags(args.trim());
        let target = if path_arg.is_empty() {
            match self.resolve_path(&self.current_dir.clone()) {
                Some(p) => p,
                None => return self.send("550 Access denied\r\n").await,
            }
        } else {
            match self.resolve_path(path_arg) {
                Some(p) => p,
                None => return self.send("550 Access denied\r\n").await,
            }
        };

        if !target.is_dir() {
            return self.send("550 Directory not found\r\n").await;
        }

        self.send("150 Opening data connection\r\n").await?;

        let mut data = match self.accept_data().await {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send(&format!("425 Cannot open data connection: {}\r\n", e))
                    .await;
            }
        };

        let mut listing = String::new();
        if let Ok(entries) = std::fs::read_dir(&target) {
            for entry in entries.flatten() {
                listing.push_str(&entry.file_name().to_string_lossy());
                listing.push_str("\r\n");
            }
        }

        let _ = data.write_all(listing.as_bytes()).await;
        let _ = data.shutdown().await;

        self.send("226 Transfer complete\r\n").await
    }

    // --- RETR (download) ---

    async fn cmd_retr(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path = match self.resolve_path(args.trim()) {
            Some(p) => p,
            None => return self.send("550 Access denied\r\n").await,
        };
        if !path.is_file() {
            return self.send("550 File not found\r\n").await;
        }

        let file_size = path.metadata().map(|m| m.len()).unwrap_or(0);

        self.send(&format!(
            "150 Opening data connection for {} ({} bytes)\r\n",
            args.trim(),
            file_size
        ))
        .await?;

        let mut data = match self.accept_data().await {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send(&format!("425 Cannot open data connection: {}\r\n", e))
                    .await;
            }
        };

        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = data.shutdown().await;
                return self.send(&format!("550 Cannot read file: {}\r\n", e)).await;
            }
        };
        let mut reader = tokio::io::BufReader::new(file);

        match tokio::io::copy(&mut reader, &mut data).await {
            Ok(_) => {
                let _ = data.shutdown().await;
                self.log(format!("RETR {} ({} bytes)", args.trim(), file_size));
                self.send("226 Transfer complete\r\n").await
            }
            Err(e) => {
                let _ = data.shutdown().await;
                self.send(&format!("550 Transfer error: {}\r\n", e)).await
            }
        }
    }

    // --- STOR (upload) ---

    async fn cmd_stor(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path = match self.resolve_path(args.trim()) {
            Some(p) => p,
            None => return self.send("550 Access denied\r\n").await,
        };

        if let Some(parent) = path.parent() {
            if !parent.is_dir() {
                return self.send("550 Directory not found\r\n").await;
            }
        }

        self.send("150 Ready to receive data\r\n").await?;

        let mut data = match self.accept_data().await {
            Ok(d) => d,
            Err(e) => {
                return self
                    .send(&format!("425 Cannot open data connection: {}\r\n", e))
                    .await;
            }
        };

        let file = match tokio::fs::File::create(&path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = data.shutdown().await;
                return self.send(&format!("550 Cannot create file: {}\r\n", e)).await;
            }
        };
        let mut writer = tokio::io::BufWriter::new(file);

        match tokio::io::copy(&mut data, &mut writer).await {
            Ok(bytes) => {
                let _ = AsyncWriteExt::shutdown(&mut writer).await;
                let _ = data.shutdown().await;
                self.log(format!("STOR {} ({} bytes)", args.trim(), bytes));
                self.send("226 Transfer complete\r\n").await
            }
            Err(e) => {
                let _ = AsyncWriteExt::shutdown(&mut writer).await;
                let _ = data.shutdown().await;
                let _ = tokio::fs::remove_file(&path).await;
                self.send(&format!("550 Error receiving data: {}\r\n", e)).await
            }
        }
    }

    // --- SIZE ---

    async fn cmd_size(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path = match self.resolve_path(args.trim()) {
            Some(p) => p,
            None => return self.send("550 Access denied\r\n").await,
        };
        match path.metadata() {
            Ok(meta) if meta.is_file() => {
                self.send(&format!("213 {}\r\n", meta.len())).await
            }
            _ => self.send("550 File not found\r\n").await,
        }
    }

    // --- DELE ---

    async fn cmd_dele(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path = match self.resolve_path(args.trim()) {
            Some(p) => p,
            None => return self.send("550 Access denied\r\n").await,
        };
        if !path.is_file() {
            return self.send("550 File not found\r\n").await;
        }

        match std::fs::remove_file(&path) {
            Ok(_) => {
                self.log(format!("DELE {}", args.trim()));
                self.send("250 File deleted\r\n").await
            }
            Err(e) => self.send(&format!("550 Cannot delete: {}\r\n", e)).await,
        }
    }

    // --- MKD / RMD ---

    async fn cmd_mkd(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path = match self.resolve_path(args.trim()) {
            Some(p) => p,
            None => return self.send("550 Access denied\r\n").await,
        };
        match std::fs::create_dir(&path) {
            Ok(_) => {
                let ftp_path = self.normalize_ftp_path(args.trim());
                self.send(&format!("257 \"{}\" created\r\n", ftp_path)).await
            }
            Err(e) => self.send(&format!("550 Cannot create directory: {}\r\n", e)).await,
        }
    }

    async fn cmd_rmd(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path = match self.resolve_path(args.trim()) {
            Some(p) => p,
            None => return self.send("550 Access denied\r\n").await,
        };
        if !path.is_dir() {
            return self.send("550 Directory not found\r\n").await;
        }

        match std::fs::remove_dir(&path) {
            Ok(_) => self.send("250 Directory removed\r\n").await,
            Err(e) => self.send(&format!("550 Cannot remove directory: {}\r\n", e)).await,
        }
    }

    // --- RNFR / RNTO (rename) ---

    async fn cmd_rnfr(&mut self, args: &str) -> io::Result<()> {
        if !self.require_auth() {
            return self.send("530 Not logged in\r\n").await;
        }

        let path = match self.resolve_path(args.trim()) {
            Some(p) => p,
            None => return self.send("550 Access denied\r\n").await,
        };
        if path.exists() {
            self.send("350 Ready for RNTO\r\n").await
        } else {
            self.send("550 File not found\r\n").await
        }
    }

    async fn cmd_rnto(&mut self, _args: &str) -> io::Result<()> {
        self.send("502 RNTO not fully implemented\r\n").await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn strip_ls_flags(args: &str) -> &str {
    let mut s = args;
    while s.starts_with('-') {
        match s.find(|c: char| c == ' ') {
            Some(i) => s = s[i + 1..].trim_start(),
            None => return "",
        }
    }
    s
}

fn format_list_entry(entry: &std::fs::DirEntry, meta: &std::fs::Metadata) -> String {
    let is_dir = meta.is_dir();

    let perms = if is_dir {
        "drwxr-xr-x"
    } else if meta.permissions().readonly() {
        "-r--r--r--"
    } else {
        "-rw-r--r--"
    };

    let size = meta.len();

    let modified: DateTime<Local> = meta
        .modified()
        .unwrap_or(std::time::SystemTime::now())
        .into();

    let now = Local::now();
    let date_str = if modified.year() == now.year() {
        modified.format("%b %d %H:%M").to_string()
    } else {
        modified.format("%b %d  %Y").to_string()
    };

    let name = entry.file_name();
    let name_str = name.to_string_lossy();

    format!(
        "{} 1 ftp ftp {:>13} {} {}",
        perms, size, date_str, name_str
    )
}

/// Decode bytes as UTF-8, falling back to the system codepage (e.g. Shift-JIS).
fn bytes_to_string(bytes: &[u8]) -> String {
    match String::from_utf8(bytes.to_vec()) {
        Ok(s) => s,
        Err(_) => decode_system_codepage(bytes),
    }
}

#[cfg(windows)]
fn decode_system_codepage(bytes: &[u8]) -> String {
    use std::ptr;

    extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte_str: *const u8,
            multi_byte_len: i32,
            wide_char_str: *mut u16,
            wide_char_len: i32,
        ) -> i32;
    }

    const CP_ACP: u32 = 0; // System default ANSI code page

    unsafe {
        let wide_len = MultiByteToWideChar(
            CP_ACP, 0,
            bytes.as_ptr(), bytes.len() as i32,
            ptr::null_mut(), 0,
        );
        if wide_len <= 0 {
            return String::from_utf8_lossy(bytes).to_string();
        }
        let mut wide = vec![0u16; wide_len as usize];
        MultiByteToWideChar(
            CP_ACP, 0,
            bytes.as_ptr(), bytes.len() as i32,
            wide.as_mut_ptr(), wide_len,
        );
        String::from_utf16_lossy(&wide)
    }
}

#[cfg(not(windows))]
fn decode_system_codepage(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}
