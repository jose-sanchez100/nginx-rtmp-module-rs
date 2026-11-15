//! A small HTTP/1.x server for the `http {}` block.
//!
//! nginx-rtmp's statistics and control modules are nginx HTTP modules,
//! enabled with `rtmp_stat` / `rtmp_control` in an `http {}` location.
//! This server reads the same configuration but only starts the `http`
//! servers that use one of them. Other locations of those servers serve
//! static files from `root`, which is what `stat.xsl` needs. Every
//! response closes the connection.

use crate::config::{HttpLocation, HttpServerConf};
use crate::live::Registry;
use crate::session::ServerCtx;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info};

/// What an HTTP server block can reach.
pub struct HttpCtx {
    pub conf: HttpServerConf,
    /// Every `rtmp` server block, in configuration order (the `srv`
    /// argument of the control module indexes this list).
    pub servers: Arc<Vec<Arc<ServerCtx>>>,
    pub registry: Arc<Registry>,
    pub prefix: Option<PathBuf>,
}

const MAX_HEADER: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);

pub async fn accept_loop(l: TcpListener, ctx: Arc<HttpCtx>) {
    loop {
        match l.accept().await {
            Ok((sock, addr)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(sock, addr, &ctx).await {
                        debug!("http {addr}: {e}");
                    }
                });
            }
            Err(e) => {
                error!("http accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self { status, content_type, body: body.into() }
    }

    fn error(status: u16) -> Self {
        let text = format!("{status} {}\n", reason(status));
        Self::new(status, "text/plain", text)
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Not Allowed",
        414 => "Request-URI Too Large",
        500 => "Internal Server Error",
        _ => "",
    }
}

async fn serve(mut sock: TcpStream, addr: SocketAddr, ctx: &HttpCtx) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let head_end = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(Some(i));
            }
            if buf.len() > MAX_HEADER {
                return Ok(None);
            }
            let mut chunk = [0u8; 2048];
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed before the request"));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"))??;

    let (head_only, resp) = match head_end {
        None => (false, Response::error(414)),
        Some(end) => {
            let head = String::from_utf8_lossy(&buf[..end]);
            let line = head.lines().next().unwrap_or("");
            let mut parts = line.split_ascii_whitespace();
            match (parts.next(), parts.next()) {
                (Some(method), Some(target)) => {
                    debug!("http {addr}: {method} {target}");
                    (method == "HEAD", handle(ctx, addr, method, target).await)
                }
                _ => (false, Response::error(400)),
            }
        }
    };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nServer: rtmp-server/{}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp.status,
        reason(resp.status),
        env!("CARGO_PKG_VERSION"),
        resp.content_type,
        resp.body.len()
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(&resp.body);
    }
    tokio::time::timeout(TIMEOUT, sock.write_all(&out))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "response timed out"))??;
    let _ = sock.shutdown().await;
    Ok(())
}

/// `%XX` decoding; `None` for invalid escapes or UTF-8.
fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Query argument, matched without case like `ngx_http_arg`, decoded.
fn query_arg(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        k.eq_ignore_ascii_case(name).then(|| decode(v).unwrap_or_else(|| v.to_string()))
    })
}

async fn handle(ctx: &HttpCtx, addr: SocketAddr, method: &str, target: &str) -> Response {
    // absolute-form: http://host/path
    let target = match target.find("://") {
        Some(i) => target[i + 3..].find('/').map_or("/", |j| &target[i + 3 + j..]),
        None => target,
    };
    let (raw_path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some(path) = decode(raw_path) else { return Response::error(400) };
    if !path.starts_with('/') || path.split(['/', '\\']).any(|seg| seg == "..") {
        return Response::error(400);
    }
    let loc = ctx.conf.find(&path);
    if !loc.allowed(addr.ip()) {
        info!("http {addr}: access forbidden to '{path}'");
        return Response::error(403);
    }
    if loc.stat != 0 {
        let body = crate::stat::xml(&ctx.servers, &ctx.registry, loc.stat, &loc.stylesheet);
        return Response::new(200, "text/xml", body);
    }
    if loc.control != 0 {
        let arg = |name: &str| query_arg(query, name);
        return match crate::control::handle(&ctx.servers, loc.control, &path, &arg).await {
            Some(r) => Response::new(r.status, "text/plain", r.body),
            None => Response::error(404),
        };
    }
    if method != "GET" && method != "HEAD" {
        return Response::error(405);
    }
    static_file(ctx.prefix.as_deref(), loc, &path)
}

fn static_file(prefix: Option<&Path>, loc: &HttpLocation, path: &str) -> Response {
    let mut file = crate::access_log::resolve_path(prefix, &loc.root);
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        // Only plain names: no drive letters, roots or separators that
        // would take the path out of `root` (on Windows "C:" or "\").
        let mut c = Path::new(seg).components();
        if !matches!((c.next(), c.next()), (Some(std::path::Component::Normal(_)), None)) || seg.contains(['\\', ':']) {
            return Response::error(400);
        }
        file.push(seg);
    }
    if path.ends_with('/') {
        file.push("index.html");
    }
    match std::fs::read(&file) {
        Ok(body) => Response::new(200, mime(&file), body),
        Err(e) => {
            debug!("http: '{}': {e}", file.display());
            Response::error(404)
        }
    }
}

fn mime(p: &Path) -> &'static str {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "xml" => "text/xml",
        // browsers apply the stylesheet of the stat page only when it has
        // an XML type
        "xsl" | "xslt" => "text/xsl",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "txt" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "m3u8" => "application/vnd.apple.mpegurl",
        "ts" => "video/mp2t",
        "mp4" => "video/mp4",
        "flv" => "video/x-flv",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_and_decoding() {
        assert_eq!(query_arg("app=live&Name=a%20b", "name").as_deref(), Some("a b"));
        assert_eq!(query_arg("app=live", "name"), None);
        assert_eq!(decode("/a%2Fb").as_deref(), Some("/a/b"));
        assert_eq!(decode("/bad%2"), None);
    }
}
