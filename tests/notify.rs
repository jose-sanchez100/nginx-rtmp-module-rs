//! on_connect / on_publish / on_play / on_*_done / on_update / on_disconnect
//! against a mock HTTP callback server.

mod common;

use common::{Client, av};
use nginx_rtmp::amf::Amf0;
use nginx_rtmp::config::Config;
use nginx_rtmp::live::Registry;
use nginx_rtmp::message::*;
use nginx_rtmp::server::Server;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};

const AVC_HDR: &[u8] = &[0x17, 0x00, 0, 0, 0, 0xAA];
const KEY: &[u8] = &[0x17, 0x01, 0, 0, 0, 7];

#[derive(Debug, Clone)]
struct Req {
    method: String,
    /// Path without the query string.
    path: String,
    /// Query string (GET) or body (POST), parsed.
    params: HashMap<String, String>,
}

impl Req {
    fn get(&self, k: &str) -> &str {
        self.params.get(k).map(String::as_str).unwrap_or("")
    }
}

/// Status code and optional `Location` per path.
type Replies = HashMap<String, (u16, Option<String>)>;

/// Mock callback server: answers each path with a configured status line and
/// optional `Location`, default `200 OK`, and records every request.
#[derive(Clone, Default)]
struct Hooks {
    reqs: Arc<Mutex<Vec<Req>>>,
    replies: Arc<Mutex<Replies>>,
}

impl Hooks {
    async fn start() -> (Hooks, SocketAddr) {
        let hooks = Hooks::default();
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let h = hooks.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head, body) = loop {
                        let n = s.read(&mut chunk).await.unwrap();
                        assert!(n > 0, "connection closed mid-request");
                        buf.extend_from_slice(&chunk[..n]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                        let head = String::from_utf8(buf[..end].to_vec()).unwrap();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .map(|v| v.parse().unwrap())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break (head, String::from_utf8(buf[end + 4..end + 4 + len].to_vec()).unwrap());
                        }
                    };
                    let line = head.lines().next().unwrap();
                    let mut parts = line.split(' ');
                    let method = parts.next().unwrap().to_string();
                    let target = parts.next().unwrap();
                    assert_eq!(parts.next(), Some("HTTP/1.0"));
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let form = if method == "POST" { body.as_str() } else { query };
                    let params = form
                        .split('&')
                        .filter(|p| !p.is_empty())
                        .map(|p| {
                            let (k, v) = p.split_once('=').unwrap_or((p, ""));
                            (k.to_string(), unescape(v))
                        })
                        .collect();
                    let req = Req { method, path: path.to_string(), params };
                    let (code, loc) = h.replies.lock().unwrap().get(&req.path).cloned().unwrap_or((200, None));
                    h.reqs.lock().unwrap().push(req);
                    let mut reply = format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\n");
                    if let Some(loc) = loc {
                        reply.push_str(&format!("Location: {loc}\r\n"));
                    }
                    reply.push_str("\r\n");
                    let _ = s.write_all(reply.as_bytes()).await;
                });
            }
        });
        (hooks, addr)
    }

    fn reply(&self, path: &str, code: u16, location: Option<&str>) {
        self.replies.lock().unwrap().insert(path.into(), (code, location.map(Into::into)));
    }

    fn calls(&self, path: &str) -> Vec<Req> {
        self.reqs.lock().unwrap().iter().filter(|r| r.path == path).cloned().collect()
    }

    async fn wait_call(&self, path: &str) -> Req {
        timeout(Duration::from_secs(10), async {
            loop {
                if let Some(r) = self.calls(path).into_iter().next() {
                    return r;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no call to {path}"))
    }
}

fn unescape(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

async fn start(conf: &str) -> (SocketAddr, Arc<Registry>) {
    let server = Server::bind(Config::parse(conf, "test").unwrap()).await.unwrap();
    let addr = server.local_addrs()[0];
    let reg = server.registry.clone();
    tokio::spawn(server.run());
    (addr, reg)
}

fn stream_state(reg: &Registry, key: &str) -> Option<(bool, usize)> {
    reg.list().into_iter().find(|(k, _, _)| k == key).map(|(_, p, n)| (p, n))
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    timeout(Duration::from_secs(10), async {
        while !cond() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Read until the connection closes, failing if a status message shows up.
async fn expect_closed_silently(c: &mut Client) {
    timeout(Duration::from_secs(10), async {
        while let Some(m) = c.recv().await {
            assert_ne!(m.type_id, MSG_AMF_CMD, "no status expected");
        }
    })
    .await
    .expect("connection should be closed");
}

#[tokio::test]
async fn publish_lifecycle_callbacks() {
    let (hooks, http) = Hooks::start().await;
    let (rtmp, reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0;
            on_connect http://{http}/connect;
            on_disconnect http://{http}/disconnect;
            application live {{
                live on;
                on_publish http://{http}/publish;
                on_publish_done http://{http}/publish_done;
                on_done http://{http}/done;
                on_play_done http://{http}/play_done;
            }}
        }} }}"
    ))
    .await;

    let mut c = Client::ready(rtmp, "live").await;
    let r = hooks.wait_call("/connect").await;
    assert_eq!((r.method.as_str(), r.get("call"), r.get("app")), ("POST", "connect", "live"));
    assert_eq!(r.get("tcurl"), format!("rtmp://{rtmp}/live"));
    assert_eq!(r.get("addr"), "127.0.0.1");

    c.publish("cam?key=s3cret&x=1").await;
    assert_eq!(c.expect_status().await, "NetStream.Publish.Start");
    let r = hooks.wait_call("/publish").await;
    assert_eq!(r.get("call"), "publish");
    assert_eq!(r.get("name"), "cam");
    assert_eq!(r.get("type"), "live");
    assert_eq!(r.get("key"), "s3cret");
    assert_eq!(r.get("x"), "1");
    assert_eq!(r.get("app"), "live");
    assert!(!r.get("clientid").is_empty());
    assert_eq!(stream_state(&reg, "0/live/cam"), Some((true, 0)));

    c.send(amf_cmd(8, 1, &[Amf0::str("deleteStream"), Amf0::Number(4.0), Amf0::Null, Amf0::Number(1.0)])).await;
    assert_eq!(c.expect_status().await, "NetStream.Unpublish.Success");
    let r = hooks.wait_call("/publish_done").await;
    assert_eq!((r.get("call"), r.get("name"), r.get("key")), ("publish_done", "cam", "s3cret"));
    let r = hooks.wait_call("/done").await;
    assert_eq!((r.get("call"), r.get("name")), ("done", "cam"));

    drop(c);
    let r = hooks.wait_call("/disconnect").await;
    assert_eq!((r.get("call"), r.get("app")), ("disconnect", "live"));
    // Done callbacks fire once per stream, not again on disconnect.
    sleep(Duration::from_millis(200)).await;
    assert_eq!(hooks.calls("/publish_done").len(), 1);
    assert_eq!(hooks.calls("/done").len(), 1);
    assert!(hooks.calls("/play_done").is_empty());
}

#[tokio::test]
async fn publish_rejected() {
    let (hooks, http) = Hooks::start().await;
    hooks.reply("/publish", 403, None);
    let (rtmp, reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on; on_publish http://{http}/publish; on_done http://{http}/done;
        }} }} }}"
    ))
    .await;
    let mut c = Client::ready(rtmp, "live").await;
    c.publish("cam").await;
    expect_closed_silently(&mut c).await;
    assert!(stream_state(&reg, "0/live/cam").is_none());
    // A rejected publish is not reported as done.
    sleep(Duration::from_millis(200)).await;
    assert!(hooks.calls("/done").is_empty());
}

#[tokio::test]
async fn callback_server_down_rejects() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let (rtmp, _) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on; on_play http://127.0.0.1:{port}/play;
        }} }} }}"
    ))
    .await;
    let mut c = Client::ready(rtmp, "live").await;
    c.play("cam").await;
    expect_closed_silently(&mut c).await;
}

#[tokio::test]
async fn connect_rejected_and_redirected() {
    let (hooks, http) = Hooks::start().await;
    hooks.reply("/connect", 302, Some("live"));
    let (rtmp, _) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; on_connect http://{http}/connect; notify_method get;
            application live {{ live on; }}
        }} }}"
    ))
    .await;

    // "legacy" does not exist, but the callback redirects it to "live".
    let mut c = Client::connect(rtmp, "legacy?token=t").await;
    c.expect_amf("_result").await;
    let r = hooks.wait_call("/connect").await;
    assert_eq!((r.method.as_str(), r.get("app"), r.get("token")), ("GET", "legacy", "t"));

    hooks.reply("/connect", 401, None);
    let mut c = Client::connect(rtmp, "live").await;
    assert!(c.recv().await.is_none(), "rejected connect must close the connection");
}

#[tokio::test]
async fn play_redirect_renames_stream() {
    let (hooks, http) = Hooks::start().await;
    hooks.reply("/play", 302, Some("real"));
    let (rtmp, reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on; on_play http://{http}/play; on_play_done http://{http}/play_done;
        }} }} }}"
    ))
    .await;

    let mut publ = Client::ready(rtmp, "live").await;
    publ.publish("real").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    publ.send(av(MSG_VIDEO, 0, AVC_HDR)).await;

    let mut play = Client::ready(rtmp, "live").await;
    play.play("alias").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    assert_eq!(hooks.wait_call("/play").await.get("name"), "alias");
    wait_for("player on the real stream", || stream_state(&reg, "0/live/real") == Some((true, 1))).await;
    assert!(stream_state(&reg, "0/live/alias").is_none());
    publ.send(av(MSG_VIDEO, 40, KEY)).await;
    assert_eq!(&play.expect_av().await.payload[..], AVC_HDR);

    // play_done reports the name the client asked for.
    drop(play);
    assert_eq!(hooks.wait_call("/play_done").await.get("name"), "alias");
}

#[tokio::test]
async fn publish_redirect_to_rtmp_pushes() {
    let (origin, origin_reg) =
        start("rtmp { server { listen 127.0.0.1:0; application origin { live on; } } }").await;
    let (hooks, http) = Hooks::start().await;
    hooks.reply("/publish", 302, Some(&format!("rtmp://{origin}/origin/fromhook")));
    let (edge, edge_reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on; on_publish http://{http}/publish; notify_relay_redirect on;
        }} }} }}"
    ))
    .await;

    let mut publ = Client::ready(edge, "live").await;
    publ.publish("cam").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    wait_for("push on origin", || stream_state(&origin_reg, "0/origin/fromhook").is_some_and(|s| s.0)).await;
    // notify_relay_redirect: the local stream is named after the URL's MD5.
    let local: Vec<_> = edge_reg.list().into_iter().map(|(k, _, _)| k).collect();
    assert_eq!(local.len(), 1);
    let name = local[0].strip_prefix("0/live/").unwrap();
    assert_eq!(name.len(), 32, "{name}");
    assert!(name.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[tokio::test]
async fn play_redirect_to_rtmp_pulls() {
    let (origin, origin_reg) =
        start("rtmp { server { listen 127.0.0.1:0; application origin { live on; } } }").await;
    let (hooks, http) = Hooks::start().await;
    hooks.reply("/play", 302, Some(&format!("rtmp://{origin}/origin/src")));
    let (edge, edge_reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on; on_play http://{http}/play;
        }} }} }}"
    ))
    .await;

    let mut publ = Client::ready(origin, "origin").await;
    publ.publish("src").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");

    let mut play = Client::ready(edge, "live").await;
    play.play("cam").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    wait_for("pull on origin", || stream_state(&origin_reg, "0/origin/src") == Some((true, 1))).await;
    wait_for("pull publishing on edge", || stream_state(&edge_reg, "0/live/cam") == Some((true, 1))).await;
}

#[tokio::test]
async fn update_failure_drops_session() {
    let (hooks, http) = Hooks::start().await;
    let (rtmp, reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on; on_update http://{http}/update; notify_update_timeout 150ms;
        }} }} }}"
    ))
    .await;
    let mut c = Client::ready(rtmp, "live").await;
    c.publish("cam?k=v").await;
    assert_eq!(c.expect_status().await, "NetStream.Publish.Start");
    c.send(av(MSG_VIDEO, 1234, AVC_HDR)).await;

    let r = hooks.wait_call("/update").await;
    assert_eq!((r.get("call"), r.get("name"), r.get("k")), ("update_publish", "cam", "v"));
    assert_eq!(r.get("timestamp"), "1234");
    assert!(r.get("time").parse::<u64>().is_ok());

    // Still alive after a few successful updates.
    wait_for("several updates", || hooks.calls("/update").len() >= 3).await;
    assert_eq!(stream_state(&reg, "0/live/cam"), Some((true, 0)));

    hooks.reply("/update", 500, None);
    timeout(Duration::from_secs(10), async { while c.recv().await.is_some() {} })
        .await
        .expect("failed update must drop the session");
    wait_for("stream to go away", || stream_state(&reg, "0/live/cam").is_none()).await;
}
