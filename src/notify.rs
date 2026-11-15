//! HTTP callbacks: `on_connect`, `on_publish`, `on_play`, `on_*_done`,
//! `on_update`, `on_disconnect` (`ngx_rtmp_notify_module.c`) and the
//! minimal HTTP/1.0 client behind them (`ngx_rtmp_netcall_module.c`).
//!
//! Requests are `application/x-www-form-urlencoded`, sent either as a POST
//! body or as a GET query string, with `Connection: Close`. Only the status
//! class and the `Location` header of the reply are looked at:
//!
//! - 2xx: continue;
//! - 3xx: continue, using `Location` as a new app (connect), a new stream
//!   name, or an `rtmp://` URL to push to / pull from (publish / play);
//! - anything else, or a network error: the session is closed.

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Upper bound on the reply we buffer; only the status line and headers matter.
const MAX_REPLY: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Method {
    Get,
    #[default]
    Post,
}

/// Callback URL (`http://host[:port]/uri`, the scheme is optional).
#[derive(Debug, Clone, PartialEq)]
pub struct NotifyUrl {
    /// As written in the configuration, for logs.
    pub raw: String,
    pub host: String,
    pub port: u16,
    /// Path and optional query, always starting with `/`.
    pub uri: String,
}

impl NotifyUrl {
    pub fn parse(url: &str) -> Option<Self> {
        let rest = if url.len() >= 7 && url[..7].eq_ignore_ascii_case("http://") { &url[7..] } else { url };
        let (hostport, uri) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
            let (h, after) = h.split_once(']')?;
            match after.strip_prefix(':') {
                Some(p) => (h.to_string(), p.parse().ok()?),
                None if after.is_empty() => (h.to_string(), 80),
                None => return None,
            }
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()?),
                None => (hostport.to_string(), 80),
            }
        };
        if host.is_empty() {
            return None;
        }
        Some(NotifyUrl { raw: url.to_string(), host, port, uri: uri.to_string() })
    }

    /// Value of the `Host` header.
    fn host_header(&self) -> String {
        let h = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == 80 { h } else { format!("{h}:{}", self.port) }
    }
}

/// Application-level callbacks (`ngx_rtmp_notify_app_conf_t`).
#[derive(Debug, Clone, Default)]
pub struct AppNotify {
    pub on_play: Option<Arc<NotifyUrl>>,
    pub on_publish: Option<Arc<NotifyUrl>>,
    pub on_play_done: Option<Arc<NotifyUrl>>,
    pub on_publish_done: Option<Arc<NotifyUrl>>,
    pub on_done: Option<Arc<NotifyUrl>>,
    pub on_record_done: Option<Arc<NotifyUrl>>,
    pub on_update: Option<Arc<NotifyUrl>>,
    pub method: Method,
    /// `notify_update_timeout` (default 30s); zero disables `on_update`.
    pub update_timeout: Duration,
    /// `notify_update_strict`: also drop the session on 3xx or network errors.
    pub update_strict: bool,
    /// `notify_relay_redirect`: rename the local stream after a redirect to
    /// an `rtmp://` URL to the MD5 hex digest of that URL.
    pub relay_redirect: bool,
}

impl AppNotify {
    pub fn new() -> Self {
        Self { update_timeout: Duration::from_secs(30), ..Default::default() }
    }

    /// Any callback configured (`nacf->active`).
    pub fn active(&self) -> bool {
        [
            &self.on_play,
            &self.on_publish,
            &self.on_play_done,
            &self.on_publish_done,
            &self.on_done,
            &self.on_record_done,
            &self.on_update,
        ]
        .iter()
        .any(|u| u.is_some())
    }
}

/// Server-level callbacks (`ngx_rtmp_notify_srv_conf_t`).
#[derive(Debug, Clone, Default)]
pub struct SrvNotify {
    pub on_connect: Option<Arc<NotifyUrl>>,
    pub on_disconnect: Option<Arc<NotifyUrl>>,
    pub method: Method,
}

/// Outcome of a callback.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// 2xx
    Ok,
    /// 3xx, with the `Location` header if present.
    Redirect(Option<String>),
    /// 4xx/5xx, unparsable reply or network error.
    Error(String),
}

/// `application/x-www-form-urlencoded` parameter list.
#[derive(Debug, Clone, Default)]
pub struct Form(String);

impl Form {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `key=value`, escaping the value.
    pub fn add(&mut self, key: &str, value: impl AsRef<str>) -> &mut Self {
        if !self.0.is_empty() {
            self.0.push('&');
        }
        self.0.push_str(key);
        self.0.push('=');
        escape_into(&mut self.0, value.as_ref());
        self
    }

    /// Append client-supplied arguments (`?a=1&b=2` of the stream or app
    /// name) verbatim, as nginx does.
    pub fn raw(&mut self, args: &str) -> &mut Self {
        if !args.is_empty() {
            if !self.0.is_empty() {
                self.0.push('&');
            }
            self.0.push_str(args);
        }
        self
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Percent-encode everything but unreserved characters and a few that are
/// harmless inside a form value (nginx's `NGX_ESCAPE_ARGS` is similar).
fn escape_into(out: &mut String, v: &str) {
    for &b in v.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:@!$'()*,".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// Build the raw HTTP/1.0 request (`ngx_rtmp_netcall_http_format_request`).
pub fn format_request(url: &NotifyUrl, method: Method, form: &Form) -> Vec<u8> {
    let (line, body) = match method {
        Method::Get => {
            let sep = if url.uri.contains('?') { '&' } else { '?' };
            (format!("GET {}{sep}{}", url.uri, form.as_str()), "")
        }
        Method::Post => (format!("POST {}", url.uri), form.as_str()),
    };
    format!(
        "{line} HTTP/1.0\r\nHost: {}\r\nContent-Type: application/x-www-form-urlencoded\r\n\
         Connection: Close\r\nContent-Length: {}\r\n\r\n{body}",
        url.host_header(),
        body.len()
    )
    .into_bytes()
}

/// Interpret a raw HTTP reply (`ngx_rtmp_notify_parse_http_retcode` and
/// `ngx_rtmp_notify_parse_http_header`).
pub fn parse_reply(buf: &[u8]) -> Reply {
    // nginx looks at the 10th byte: "HTTP/1.x NNN".
    let Some(&c) = buf.get(9) else {
        return Reply::Error("empty or broken HTTP response".into());
    };
    match c {
        b'2' => Reply::Ok,
        b'3' => Reply::Redirect(header(buf, "location")),
        b'0'..=b'9' => Reply::Error(format!("HTTP {}xx", c as char)),
        _ => Reply::Error("invalid HTTP status".into()),
    }
}

fn header(buf: &[u8], name: &str) -> Option<String> {
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]);
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string()).filter(|v| !v.is_empty())
    })
}

/// Send one callback and wait for the reply (bounded by `timeout`, the
/// `netcall_timeout` directive).
pub async fn call(url: &NotifyUrl, method: Method, form: &Form, timeout: Duration) -> Reply {
    let req = format_request(url, method, form);
    let fut = async {
        let mut s = TcpStream::connect((url.host.as_str(), url.port)).await?;
        s.write_all(&req).await?;
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            // Status line and headers are all we need.
            if buf.len() >= MAX_REPLY || buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok::<_, std::io::Error>(buf)
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(buf)) => parse_reply(&buf),
        Ok(Err(e)) => Reply::Error(format!("{}: {e}", url.raw)),
        Err(_) => Reply::Error(format!("{}: timed out", url.raw)),
    }
}

/// Local stream name for `notify_relay_redirect` (`ngx_rtmp_notify_set_name`).
pub fn relay_redirect_name(url: &str) -> String {
    use md5::{Digest, Md5};
    Md5::digest(url.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = NotifyUrl::parse("http://auth.local:8080/rtmp/publish?x=1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.uri.as_str()), ("auth.local", 8080, "/rtmp/publish?x=1"));
        let u = NotifyUrl::parse("127.0.0.1").unwrap();
        assert_eq!((u.port, u.uri.as_str()), (80, "/"));
        assert!(NotifyUrl::parse("http://:80/x").is_none());
    }

    #[test]
    fn form_and_request() {
        let mut f = Form::new();
        f.add("app", "live").add("tcurl", "rtmp://h/live").add("name", "a b&c").raw("key=s%20x");
        assert_eq!(f.as_str(), "app=live&tcurl=rtmp://h/live&name=a%20b%26c&key=s%20x");

        let u = NotifyUrl::parse("http://h:81/cb?t=1").unwrap();
        let get = String::from_utf8(format_request(&u, Method::Get, &f)).unwrap();
        assert!(get.starts_with("GET /cb?t=1&app=live&"), "{get}");
        assert!(get.contains("Host: h:81\r\n"));
        assert!(get.ends_with("Content-Length: 0\r\n\r\n"));

        let post = String::from_utf8(format_request(&u, Method::Post, &f)).unwrap();
        assert!(post.starts_with("POST /cb?t=1 HTTP/1.0\r\n"));
        assert!(post.ends_with(&format!("Content-Length: {}\r\n\r\n{}", f.as_str().len(), f.as_str())));
    }

    #[test]
    fn reply_parsing() {
        assert_eq!(parse_reply(b"HTTP/1.1 200 OK\r\n\r\n"), Reply::Ok);
        assert_eq!(
            parse_reply(b"HTTP/1.1 302 Found\r\nX: y\r\nlocation:  other\r\n\r\nbody"),
            Reply::Redirect(Some("other".into()))
        );
        assert_eq!(parse_reply(b"HTTP/1.0 301 Moved\r\n\r\n"), Reply::Redirect(None));
        assert!(matches!(parse_reply(b"HTTP/1.1 403 Forbidden\r\n\r\n"), Reply::Error(_)));
        assert!(matches!(parse_reply(b""), Reply::Error(_)));
    }

    #[test]
    fn md5_name() {
        assert_eq!(relay_redirect_name(""), "d41d8cd98f00b204e9800998ecf8427e");
    }
}
