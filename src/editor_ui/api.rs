//! Client of the editor API served by [`nginx_rtmp::editor`] in the parent
//! process: loading, saving and reloading. Checking and converting
//! between text and tree are done in this process with
//! [`nginx_rtmp::conf_form`] and [`nginx_rtmp::conf_edit`].

use nginx_rtmp::conf_edit::Item;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Where the API is: `127.0.0.1:port` and `/<token>/`.
#[derive(Debug, Clone)]
pub struct Api {
    addr: String,
    base: String,
}

/// The file as loaded.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub path: String,
    pub text: String,
    pub hash: String,
    /// `Err` when the text cannot be shown as a form.
    pub tree: Result<Vec<Item>, String>,
}

#[derive(Debug, Clone)]
pub enum Saved {
    /// Written; then the server was reloaded (`ok`) or not, with its message.
    Done { ok: bool, message: String },
    /// The file changed on disk since it was loaded.
    Conflict,
    Failed(String),
}

/// What is saved: the form's tree or the text.
pub enum Content {
    Tree(Vec<Item>),
    Text(String),
}

impl Api {
    /// From the editor's address, `http://127.0.0.1:port/<token>/`.
    pub fn new(url: &str) -> Option<Api> {
        let rest = url.trim().strip_prefix("http://")?;
        let (addr, path) = rest.split_once('/')?;
        let base = format!("/{}/", path.trim_end_matches('/'));
        Some(Api { addr: addr.to_string(), base })
    }

    pub fn load(&self) -> Result<Loaded, String> {
        let (status, v) = self.call("GET", "load", None)?;
        if status != 200 {
            return Err(error_of(&v, status));
        }
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let tree = match v.get("parse_error").and_then(Value::as_str) {
            Some(e) => Err(e.to_string()),
            None => serde_json::from_value(v["tree"].clone()).map_err(|e| format!("invalid tree: {e}")),
        };
        Ok(Loaded { path: s("path"), text: s("text"), hash: s("hash"), tree })
    }

    pub fn save(&self, content: Content, hash: &str, force: bool) -> Saved {
        let body = match content {
            Content::Tree(tree) => json!({ "tree": tree, "hash": hash, "force": force }),
            Content::Text(text) => json!({ "text": text, "hash": hash, "force": force }),
        };
        match self.call("POST", "save", Some(&body)) {
            Ok((409, _)) => Saved::Conflict,
            Ok((200, v)) => Saved::Done {
                ok: v["reload"]["ok"].as_bool().unwrap_or(false),
                message: v["reload"]["message"].as_str().unwrap_or("").to_string(),
            },
            Ok((status, v)) => Saved::Failed(error_of(&v, status)),
            Err(e) => Saved::Failed(e),
        }
    }

    /// Reload the running server with the file on disk.
    pub fn reload(&self) -> Result<(bool, String), String> {
        let (status, v) = self.call("POST", "reload", Some(&json!({})))?;
        if status != 200 {
            return Err(error_of(&v, status));
        }
        Ok((v["ok"].as_bool().unwrap_or(false), v["message"].as_str().unwrap_or("").to_string()))
    }

    fn call(&self, method: &str, what: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
        let lost = |e: std::io::Error| format!("the server cannot be reached ({e}); is it still running?");
        let mut sock = TcpStream::connect(&self.addr).map_err(lost)?;
        // Saving waits for the server to reload.
        sock.set_read_timeout(Some(Duration::from_secs(60))).map_err(lost)?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let head = format!(
            "{method} {}api/{what} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.base,
            self.addr,
            body.len()
        );
        sock.write_all(head.as_bytes()).and_then(|()| sock.write_all(body.as_bytes())).map_err(lost)?;
        let mut reply = Vec::new();
        sock.read_to_end(&mut reply).map_err(lost)?;
        parse_reply(&reply)
    }
}

fn parse_reply(reply: &[u8]) -> Result<(u16, Value), String> {
    let split = reply.windows(4).position(|w| w == b"\r\n\r\n").ok_or("incomplete reply from the server")?;
    let head = String::from_utf8_lossy(&reply[..split]);
    let status = head.split_ascii_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("invalid reply from the server")?;
    let v = serde_json::from_slice(&reply[split + 4..]).unwrap_or(Value::Null);
    Ok((status, v))
}

fn error_of(v: &Value, status: u16) -> String {
    v["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("HTTP {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address() {
        let a = Api::new("http://127.0.0.1:5555/abc/\n").unwrap();
        assert_eq!((a.addr.as_str(), a.base.as_str()), ("127.0.0.1:5555", "/abc/"));
        assert!(Api::new("127.0.0.1:5555/abc/").is_none());
    }

    #[test]
    fn replies() {
        let (s, v) = parse_reply(b"HTTP/1.1 409 Conflict\r\nX: y\r\n\r\n{\"error\":\"changed\"}").unwrap();
        assert_eq!(s, 409);
        assert_eq!(error_of(&v, s), "changed");
        assert!(parse_reply(b"HTTP/1.1 200 OK\r\n").is_err());
    }
}
