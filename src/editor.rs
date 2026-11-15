//! Interactive `nginx.conf` editor: a web page served on 127.0.0.1, shown
//! in the application's window on Windows (`editor_window.rs`) or a browser.
//!
//! The page (`editor.html`) shows the configuration as a tree of blocks
//! with a form for each directive, built from [`crate::conf_schema`], and
//! a plain text view. Saving checks the configuration, writes the file
//! (keeping the previous one as `nginx.conf.bak`) and then runs the
//! `apply` callback, which reloads the running server.
//!
//! Only local connections are served, every URL carries a random token
//! and the `Host` header must name the loopback address, so other web
//! pages open in the browser cannot read or change the configuration.

use crate::conf_edit::{self, Item};
use serde_json::{Value, json};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info};

/// Called after the file is saved: reloads the server. Returns a message
/// for the page, or an error.
pub type Apply = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync>;

pub struct Editor {
    conf_path: PathBuf,
    token: String,
    apply: Apply,
}

/// A running editor.
pub struct Running {
    /// Address of the page, token included.
    pub url: String,
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const PAGE: &str = include_str!("editor.html");
const MAX_BODY: usize = 4 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// Serve the editor for `conf_path` on 127.0.0.1 (`port` 0: any free one).
pub async fn start(conf_path: PathBuf, port: u16, apply: Apply) -> std::io::Result<Running> {
    let l = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let addr = l.local_addr()?;
    let ed = Arc::new(Editor { conf_path, token: token(), apply });
    let url = format!("http://{addr}/{}/", ed.token);
    info!("configuration editor on http://{addr}/");
    let task = tokio::spawn(async move {
        loop {
            let Ok((sock, peer)) = l.accept().await else { continue };
            if !peer.ip().is_loopback() {
                continue;
            }
            let ed = ed.clone();
            tokio::spawn(async move {
                if let Err(e) = ed.serve(sock).await {
                    debug!("editor {peer}: {e}");
                }
            });
        }
    });
    Ok(Running { url, addr, task })
}

/// 128 random bits, hex (std's hasher keys come from the OS).
fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

struct Request {
    method: String,
    path: String,
    host: String,
    body: Vec<u8>,
}

async fn read_request(sock: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 64 * 1024 {
            return Ok(None);
        }
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split_ascii_whitespace();
    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let (mut host, mut len) = (String::new(), 0usize);
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "host" => host = v.trim().to_string(),
                "content-length" => len = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    if len > MAX_BODY {
        return Ok(None);
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let mut chunk = vec![0u8; (len - body.len()).min(64 * 1024)];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Some(Request { method, path, host, body }))
}

/// `Host` names the loopback interface (not a name that a foreign page
/// could have pointed at 127.0.0.1).
fn local_host(host: &str) -> bool {
    let h = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    matches!(h, "127.0.0.1" | "localhost" | "::1")
}

struct Reply {
    status: u16,
    ctype: &'static str,
    body: Vec<u8>,
}

fn json_reply(status: u16, v: Value) -> Reply {
    Reply { status, ctype: "application/json", body: v.to_string().into_bytes() }
}

fn error_reply(status: u16, msg: impl Into<String>) -> Reply {
    json_reply(status, json!({ "error": msg.into() }))
}

impl Editor {
    async fn serve(&self, mut sock: TcpStream) -> std::io::Result<()> {
        let req = match tokio::time::timeout(TIMEOUT, read_request(&mut sock)).await {
            Ok(r) => r?,
            Err(_) => return Ok(()),
        };
        let reply = match req {
            None => error_reply(400, "bad request"),
            Some(r) if !local_host(&r.host) => error_reply(403, "forbidden"),
            Some(r) => self.handle(&r).await,
        };
        let reason = match reply.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            _ => "Error",
        };
        let head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
             X-Frame-Options: DENY\r\nConnection: close\r\n\r\n",
            reply.status,
            reply.ctype,
            reply.body.len()
        );
        sock.write_all(head.as_bytes()).await?;
        sock.write_all(&reply.body).await?;
        let _ = sock.shutdown().await;
        Ok(())
    }

    async fn handle(&self, r: &Request) -> Reply {
        let Some(rest) = r.path.strip_prefix('/').and_then(|p| p.strip_prefix(self.token.as_str())) else {
            return error_reply(404, "not found");
        };
        let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        match (r.method.as_str(), rest) {
            ("GET", "/") => Reply { status: 200, ctype: "text/html; charset=utf-8", body: PAGE.as_bytes().to_vec() },
            ("GET", "") => Reply { status: 200, ctype: "text/html; charset=utf-8", body: PAGE.as_bytes().to_vec() },
            ("GET", "/api/schema") => json_reply(200, json!(crate::conf_schema::specs())),
            ("GET", "/api/load") => self.load(),
            ("POST", "/api/render") => match tree_of(&body) {
                Ok(tree) => {
                    let text = conf_edit::render(&tree);
                    json_reply(200, json!({ "text": text, "check": self.check(&text) }))
                }
                Err(e) => error_reply(400, e),
            },
            ("POST", "/api/parse") => {
                let text = body["text"].as_str().unwrap_or("");
                match conf_edit::parse(text) {
                    Ok(tree) => json_reply(200, json!({ "tree": tree, "check": self.check(text) })),
                    Err(e) => error_reply(400, e),
                }
            }
            ("POST", "/api/save") => self.save(&body).await,
            ("POST", "/api/reload") => match (self.apply)().await {
                Ok(m) => json_reply(200, json!({ "ok": true, "message": m })),
                Err(e) => json_reply(200, json!({ "ok": false, "message": e })),
            },
            _ => error_reply(404, "not found"),
        }
    }

    fn load(&self) -> Reply {
        let text = match std::fs::read_to_string(&self.conf_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return error_reply(500, format!("cannot read {}: {e}", self.conf_path.display())),
        };
        let hash = conf_edit::content_hash(&text);
        let path = self.conf_path.display().to_string();
        match conf_edit::parse(&text) {
            Ok(tree) => json_reply(200, json!({ "path": path, "text": text, "hash": hash, "tree": tree, "check": self.check(&text) })),
            Err(e) => json_reply(200, json!({ "path": path, "text": text, "hash": hash, "parse_error": e })),
        }
    }

    /// What the server would say about `text`.
    fn check(&self, text: &str) -> Value {
        match crate::config::Config::parse(text, &file_name(&self.conf_path)) {
            Ok(c) => json!({ "ok": true, "warnings": c.warnings }),
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        }
    }

    async fn save(&self, body: &Value) -> Reply {
        let text = match body.get("tree") {
            Some(_) => match tree_of(body) {
                Ok(t) => conf_edit::render(&t),
                Err(e) => return error_reply(400, e),
            },
            None => body["text"].as_str().unwrap_or("").to_string(),
        };
        if let Err(e) = crate::config::Config::parse(&text, &file_name(&self.conf_path)) {
            return error_reply(400, format!("not saved: {e}"));
        }
        let current = std::fs::read_to_string(&self.conf_path).unwrap_or_default();
        let force = body["force"].as_bool().unwrap_or(false);
        if !force && body["hash"].as_str() != Some(conf_edit::content_hash(&current).as_str()) {
            return error_reply(409, "the file was changed by another program since it was loaded");
        }
        // Keep Windows line endings if the file had them.
        let text = if current.contains("\r\n") { text.replace("\r\n", "\n").replace('\n', "\r\n") } else { text };
        if let Err(e) = write_with_backup(&self.conf_path, &current, &text) {
            return error_reply(500, format!("not saved: {e}"));
        }
        info!("configuration saved to {}", self.conf_path.display());
        let (ok, message) = match (self.apply)().await {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        };
        json_reply(200, json!({ "saved": true, "hash": conf_edit::content_hash(&text), "reload": { "ok": ok, "message": message } }))
    }
}

fn tree_of(body: &Value) -> Result<Vec<Item>, String> {
    serde_json::from_value(body["tree"].clone()).map_err(|e| format!("invalid tree: {e}"))
}

fn file_name(p: &Path) -> String {
    p.display().to_string()
}

/// Keep the previous content as `<file>.bak`, then replace the file
/// through a temporary one so it is never left half written.
fn write_with_backup(path: &Path, previous: &str, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        std::fs::write(path.with_extension("conf.bak"), previous)?;
    }
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert!(local_host("127.0.0.1:8080"));
        assert!(local_host("localhost:1"));
        assert!(local_host("[::1]:80"));
        assert!(!local_host("evil.example:8080"));
        assert!(!local_host("127.0.0.1.evil.example"));
    }
}
