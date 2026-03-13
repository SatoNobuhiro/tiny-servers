use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::common::{is_within_root, log_access, log_system, SharedLog};

const MAX_REQUEST_LINE: usize = 8192;

#[derive(Clone)]
pub struct HttpConfig {
    pub root_dir: PathBuf,
    pub port: u16,
    pub bind_addr: String,
}

pub async fn run(config: HttpConfig, log: SharedLog, mut shutdown_rx: watch::Receiver<bool>) {
    let addr = format!("{}:{}", config.bind_addr, config.port);

    let port = config.port;
    let bind_addr = config.bind_addr.clone();

    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            log_system(&log, "HTTP", &bind_addr, port,
                format!("Server started (folder: {})", config.root_dir.display()));
            l
        }
        Err(e) => {
            log_system(&log, "HTTP", &bind_addr, port, format!("Failed to bind: {}", e));
            return;
        }
    };

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let config = config.clone();
                        let log = log.clone();
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, addr, &config, &log).await;
                        });
                    }
                    Err(e) => {
                        log_system(&log, "HTTP", &bind_addr, port, format!("Accept error: {}", e));
                    }
                }
            }
        }
    }

    log_system(&log, "HTTP", &bind_addr, port, "Server listener closed");
}

async fn read_line_limited(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    limit: usize,
) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(256);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if buf.is_empty() { Ok(None) } else {
                Ok(Some(String::from_utf8_lossy(&buf).to_string()))
            };
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            reader.consume(pos + 1);
            return Ok(Some(String::from_utf8_lossy(&buf).to_string()));
        }

        let len = available.len();
        buf.extend_from_slice(available);
        reader.consume(len);

        if buf.len() > limit {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line too long"));
        }
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    config: &HttpConfig,
    log: &SharedLog,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Read request line (bounded)
    let request_line = match read_line_limited(&mut reader, MAX_REQUEST_LINE).await {
        Ok(Some(line)) => line,
        Ok(None) => return Ok(()),
        Err(_) => {
            send_response(&mut writer, 414, "URI Too Long", "text/plain", b"URI Too Long").await?;
            return Ok(());
        }
    };

    // Consume headers (bounded)
    loop {
        match read_line_limited(&mut reader, MAX_REQUEST_LINE).await {
            Ok(Some(line)) if line.trim().is_empty() => break,
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => {
                send_response(&mut writer, 431, "Request Header Fields Too Large", "text/plain", b"Header too large").await?;
                return Ok(());
            }
        }
    }

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        send_response(&mut writer, 400, "Bad Request", "text/plain", b"Bad Request").await?;
        return Ok(());
    }

    let method = parts[0];
    let raw_path = parts[1];

    if method != "GET" && method != "HEAD" {
        send_response(
            &mut writer,
            405,
            "Method Not Allowed",
            "text/plain",
            b"Method Not Allowed",
        )
        .await?;
        return Ok(());
    }

    let decoded = url_decode(raw_path);
    let clean = normalize_path(&decoded);

    log_access(log, "HTTP", &config.bind_addr, config.port, addr, format!("{} {}", method, clean));

    let fs_path = match resolve_path(&config.root_dir, &clean) {
        Some(p) => p,
        None => {
            send_response(&mut writer, 403, "Forbidden", "text/html", b"<h1>403 Forbidden</h1>")
                .await?;
            return Ok(());
        }
    };
    let head_only = method == "HEAD";

    if fs_path.is_dir() {
        let index = fs_path.join("index.html");
        if index.is_file() {
            serve_file(&mut writer, &index, head_only).await?;
        } else {
            serve_directory(&mut writer, &fs_path, &clean, head_only).await?;
        }
    } else if fs_path.is_file() {
        serve_file(&mut writer, &fs_path, head_only).await?;
    } else {
        send_response(&mut writer, 404, "Not Found", "text/html", b"<h1>404 Not Found</h1>")
            .await?;
    }

    Ok(())
}

fn resolve_path(root: &Path, url_path: &str) -> Option<PathBuf> {
    let rel = url_path.replace('\\', "/");
    let rel = rel.trim_start_matches('/');

    for component in Path::new(rel).components() {
        if matches!(component,
            std::path::Component::Prefix(_) | std::path::Component::ParentDir
        ) {
            return None;
        }
    }

    let result = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };

    // Final containment check (resolves symlinks/junctions)
    if !is_within_root(root, &result) {
        return None;
    }

    Some(result)
}

fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
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

fn url_decode(s: &str) -> String {
    let path = s.split('?').next().unwrap_or(s);
    let mut result = Vec::new();
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &path[i + 1..i + 3];
            if let Ok(val) = u8::from_str_radix(hex, 16) {
                result.push(val);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

async fn serve_file(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    path: &Path,
    head_only: bool,
) -> std::io::Result<()> {
    let content = std::fs::read(path)?;
    let content_type = guess_content_type(path);

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        content_type,
        content.len()
    );
    writer.write_all(header.as_bytes()).await?;
    if !head_only {
        writer.write_all(&content).await?;
    }
    Ok(())
}

async fn serve_directory(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    dir: &Path,
    url_path: &str,
    head_only: bool,
) -> std::io::Result<()> {
    let escaped_path = html_escape(url_path);
    let mut html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <title>Index of {p}</title>\
         <style>body{{font-family:sans-serif;margin:20px}}a{{text-decoration:none;color:#1565C0}}\
         a:hover{{text-decoration:underline}}table{{border-collapse:collapse}}\
         td{{padding:4px 16px}}</style></head>\
         <body><h2>Index of {p}</h2><hr><table>",
        p = escaped_path
    );

    if url_path != "/" {
        let parent = Path::new(url_path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/");
        html.push_str(&format!(
            "<tr><td><a href=\"{}\">../</a></td><td></td></tr>",
            html_escape(parent)
        ));
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut items: Vec<_> = entries.flatten().collect();
        items.sort_by_key(|e| e.file_name());

        for entry in items {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta
                .as_ref()
                .map(|m| {
                    if is_dir {
                        "-".to_string()
                    } else {
                        format_size(m.len())
                    }
                })
                .unwrap_or_default();

            let href = if url_path.ends_with('/') {
                format!("{}{}{}", url_path, name, if is_dir { "/" } else { "" })
            } else {
                format!("{}/{}{}", url_path, name, if is_dir { "/" } else { "" })
            };
            let display = if is_dir {
                format!("{}/", name)
            } else {
                name.clone()
            };

            html.push_str(&format!(
                "<tr><td><a href=\"{}\">{}</a></td><td align=\"right\">{}</td></tr>",
                html_escape(&href), html_escape(&display), size
            ));
        }
    }

    html.push_str("</table><hr></body></html>");
    let body = html.as_bytes();

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(header.as_bytes()).await?;
    if !head_only {
        writer.write_all(body).await?;
    }
    Ok(())
}

async fn send_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    status_text: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        status_text,
        content_type,
        body.len()
    );
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    Ok(())
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

fn guess_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "txt" | "log" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}
