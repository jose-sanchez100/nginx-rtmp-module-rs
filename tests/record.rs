//! record module: FLV files written for published (and pulled) streams,
//! and the record_done hooks.

mod common;

use common::{Client, av};
use nginx_rtmp::config::Config;
use nginx_rtmp::message::*;
use nginx_rtmp::server::Server;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const AVC_HDR: &[u8] = &[0x17, 0x00, 0, 0, 0, 0xAA];
const AAC_HDR: &[u8] = &[0xAF, 0x00, 0x12, 0x10];
const INTER: &[u8] = &[0x27, 0x01, 0, 0, 0, 1];
const KEY: &[u8] = &[0x17, 0x01, 0, 0, 0, 7];
const AUDIO: &[u8] = &[0xAF, 0x01, 0x55];

#[cfg(windows)]
const ECHO: &str = "cmd /c echo";
#[cfg(not(windows))]
const ECHO: &str = "echo";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rtmp-record-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn slash(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

async fn start(conf: &str) -> SocketAddr {
    let server = Server::bind(Config::parse(conf, "test").unwrap()).await.unwrap();
    let addr = server.local_addrs()[0];
    tokio::spawn(server.run());
    addr
}

async fn wait_until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let t = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p).unwrap_or_default().lines().map(|l| l.trim().to_string()).collect()
}

/// (type, timestamp, payload) of every tag, after checking the structure.
fn flv_tags(data: &[u8]) -> (u8, Vec<(u8, u32, Vec<u8>)>) {
    assert_eq!(&data[..3], b"FLV", "FLV signature");
    assert_eq!(&data[5..13], &[0, 0, 0, 9, 0, 0, 0, 0]);
    let mut tags = Vec::new();
    let mut p = 13;
    while p < data.len() {
        let t = data[p];
        let len = u32::from_be_bytes([0, data[p + 1], data[p + 2], data[p + 3]]) as usize;
        let ts = u32::from_be_bytes([data[p + 7], data[p + 4], data[p + 5], data[p + 6]]);
        let body = data[p + 11..p + 11 + len].to_vec();
        let size = u32::from_be_bytes(data[p + 11 + len..p + 15 + len].try_into().unwrap()) as usize;
        assert_eq!(size, 11 + len, "PreviousTagSize");
        tags.push((t, ts, body));
        p += 15 + len;
    }
    (data[4], tags)
}

/// HTTP server that answers 200 and collects request bodies / query strings.
async fn http_sink() -> (SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let g = got.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let g = g.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            g.lock().unwrap().push(text[i + 4..i + 4 + len].to_string());
                            break;
                        }
                    }
                }
                let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n").await;
            });
        }
    });
    (addr, got)
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

fn form_get(body: &str, k: &str) -> String {
    body.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(unescape).unwrap_or_default()
}

#[tokio::test]
async fn records_flv_and_runs_done_hooks() {
    let d = tmp("flv");
    let log = d.join("done.log");
    let (http, calls) = http_sink().await;
    let addr = start(&format!(
        r#"rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on;
            record all;
            record_path "{rec}";
            record_notify on;
            exec_record_done {ECHO} $recorder:$name:$filename:$basename ">>{log}";
            on_record_done http://{http}/done;
        }} }} }}"#,
        rec = slash(&d.join("rec")),
        log = slash(&log),
    ))
    .await;

    let mut publ = Client::ready(addr, "live").await;
    publ.publish("cam?key=1").await;
    assert_eq!(publ.expect_status().await, "NetStream.Record.Start");
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    publ.send(av(MSG_VIDEO, 1000, AVC_HDR)).await;
    publ.send(av(MSG_AUDIO, 1000, AAC_HDR)).await;
    publ.send(av(MSG_VIDEO, 1010, INTER)).await; // dropped: no key frame yet
    publ.send(av(MSG_AUDIO, 1020, AUDIO)).await;
    publ.send(av(MSG_VIDEO, 1040, KEY)).await;
    publ.send(av(MSG_VIDEO, 1080, INTER)).await;
    publ.send(nginx_rtmp::message::amf_cmd(
        8,
        1,
        &[nginx_rtmp::amf::Amf0::str("deleteStream"), nginx_rtmp::amf::Amf0::Number(0.0), nginx_rtmp::amf::Amf0::Null],
    ))
    .await;
    assert_eq!(publ.expect_status().await, "NetStream.Record.Stop");

    let path = d.join("rec").join("cam.flv");
    let (flags, tags) = flv_tags(&std::fs::read(&path).unwrap());
    assert_eq!(flags, 0x05, "audio and video");
    let got: Vec<(u8, u32, &[u8])> = tags.iter().map(|(t, ts, b)| (*t, *ts, &b[..])).collect();
    assert_eq!(
        got,
        [
            (MSG_VIDEO, 0, AVC_HDR),
            (MSG_AUDIO, 0, AAC_HDR),
            (MSG_AUDIO, 20, AUDIO),
            (MSG_VIDEO, 40, KEY),
            (MSG_VIDEO, 80, INTER),
        ]
    );

    let l = wait_until("exec_record_done", || Some(lines(&log)).filter(|l| !l.is_empty())).await;
    assert_eq!(l, [":cam:cam.flv:cam"]);
    let body = wait_until("on_record_done", || calls.lock().unwrap().first().cloned()).await;
    assert_eq!(form_get(&body, "call"), "record_done");
    assert_eq!(form_get(&body, "app"), "live");
    assert_eq!(form_get(&body, "name"), "cam");
    assert_eq!(form_get(&body, "key"), "1");
    assert_eq!(Path::new(&form_get(&body, "path")), path);
}

/// `recorder` blocks inherit the application settings, keep their own
/// flags, and `record_max_frames` starts a new file.
#[tokio::test]
async fn recorders_and_limits() {
    let d = tmp("recorders");
    let log = d.join("done.log");
    let addr = start(&format!(
        r#"rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on;
            record_path "{rec}";
            exec_record_done {ECHO} done $recorder $filename ">>{log}";
            recorder audio {{
                record audio;
                record_suffix .audio.flv;
            }}
            recorder keys {{
                record keyframes;
                record_suffix -%Y.flv;
                record_max_frames 2;
            }}
        }} }} }}"#,
        rec = slash(&d),
        log = slash(&log),
    ))
    .await;

    let mut publ = Client::ready(addr, "live").await;
    publ.publish("s").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    for (t, ts, p) in [
        (MSG_VIDEO, 0, AVC_HDR),
        (MSG_AUDIO, 0, AAC_HDR),
        (MSG_VIDEO, 0, KEY),
        (MSG_AUDIO, 10, AUDIO),
        (MSG_VIDEO, 20, INTER),
        (MSG_VIDEO, 40, KEY),
    ] {
        publ.send(av(t, ts, p)).await;
    }
    let year = chrono::Local::now().format("%Y").to_string();
    let keys = format!("s-{year}.flv");
    // Two key frames: the keyframes recorder closes its file by itself.
    let l = wait_until("max_frames close", || Some(lines(&log)).filter(|l| !l.is_empty())).await;
    assert_eq!(l, [format!("done keys {keys}")]);
    let (flags, tags) = flv_tags(&std::fs::read(d.join(&keys)).unwrap());
    assert_eq!(flags, 0x01, "video only");
    assert_eq!(tags.iter().map(|t| (t.0, t.1)).collect::<Vec<_>>(), [(MSG_VIDEO, 0), (MSG_VIDEO, 0), (MSG_VIDEO, 40)]);

    drop(publ);
    let mut l = wait_until("close on disconnect", || Some(lines(&log)).filter(|l| l.len() >= 2)).await;
    l.sort();
    assert_eq!(l, ["done audio s.audio.flv".to_string(), format!("done keys {keys}")]);
    let (flags, tags) = flv_tags(&std::fs::read(d.join("s.audio.flv")).unwrap());
    assert_eq!(flags, 0x04, "audio only");
    assert_eq!(tags.iter().map(|t| (t.0, t.1)).collect::<Vec<_>>(), [(MSG_AUDIO, 0), (MSG_AUDIO, 10)]);
    assert!(!d.join("s.flv").exists(), "the application itself does not record");
}

/// `record_append` continues the file and its timeline.
#[tokio::test]
async fn append_continues_timeline() {
    let d = tmp("append");
    let addr = start(&format!(
        r#"rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on; record video; record_append on; record_path "{}";
        }} }} }}"#,
        slash(&d)
    ))
    .await;
    let path = d.join("a.flv");
    for base in [5000u32, 100] {
        let mut publ = Client::ready(addr, "live").await;
        publ.publish("a").await;
        assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
        publ.send(av(MSG_VIDEO, base, AVC_HDR)).await;
        publ.send(av(MSG_VIDEO, base, KEY)).await;
        publ.send(av(MSG_VIDEO, base + 40, INTER)).await;
        drop(publ);
        let n = if base == 5000 { 3 } else { 6 };
        wait_until("file written", || {
            let data = std::fs::read(&path).ok()?;
            // Written and closed (the av mask is set on close).
            (data.len() > 13 && data[4] == 0x01 && flv_tags(&data).1.len() == n).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (_, tags) = flv_tags(&std::fs::read(&path).unwrap());
    assert_eq!(tags.iter().map(|t| t.1).collect::<Vec<_>>(), [0, 0, 40, 40, 40, 80]);
}

/// A stream pulled from another server is recorded on the edge.
#[tokio::test]
async fn records_pulled_streams() {
    let origin = start("rtmp { server { listen 127.0.0.1:0; application origin { live on; } } }").await;
    let d = tmp("pull");
    let edge = start(&format!(
        r#"rtmp {{ server {{ listen 127.0.0.1:0; application edge {{
            live on; pull rtmp://{origin}/origin; record all; record_path "{}";
        }} }} }}"#,
        slash(&d)
    ))
    .await;

    let mut publ = Client::ready(origin, "origin").await;
    publ.publish("p").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    let mut play = Client::ready(edge, "edge").await;
    play.play("p").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    tokio::time::sleep(Duration::from_millis(300)).await;
    publ.send(av(MSG_VIDEO, 0, AVC_HDR)).await;
    publ.send(av(MSG_VIDEO, 0, KEY)).await;
    loop {
        let m = play.expect_av().await;
        if &m.payload[..] == KEY {
            break;
        }
    }
    drop(play); // the pull stops with its last player
    wait_until("pulled file", || {
        let data = std::fs::read(d.join("p.flv")).ok()?;
        (data.len() > 13 && data[4] == 0x01).then_some(())
    })
    .await;
    let (_, tags) = flv_tags(&std::fs::read(d.join("p.flv")).unwrap());
    let bodies: Vec<&[u8]> = tags.iter().map(|t| &t.2[..]).collect();
    assert_eq!(bodies, [AVC_HDR, KEY]);
}
