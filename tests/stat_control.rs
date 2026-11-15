//! stat and control modules: the XML statistics page and the HTTP control
//! interface (record, drop, redirect), served from the `http {}` block.

mod common;

use common::{Client, av};
use nginx_rtmp::config::Config;
use nginx_rtmp::live::Registry;
use nginx_rtmp::message::*;
use nginx_rtmp::server::Server;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// x264 640x360 High@3.0 decoder configuration record.
const AVC_HDR: &[u8] = &[
    0x17, 0x00, 0, 0, 0, 0x01, 0x64, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x19, 0x67, 0x64, 0x00, 0x1e, 0xac, 0xd9, 0x40,
    0xa0, 0x2f, 0xf9, 0x70, 0x11, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x32, 0x0f, 0x16, 0x2d,
    0x96, 0x01, 0x00, 0x06, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0,
];
const AAC_HDR: &[u8] = &[0xAF, 0x00, 0x12, 0x10];
const KEY: &[u8] = &[0x17, 0x01, 0, 0, 0, 7];
const INTER: &[u8] = &[0x27, 0x01, 0, 0, 0, 1];
const AUDIO: &[u8] = &[0xAF, 0x01, 0x55];

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rtmp-stat-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn slash(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

struct Running {
    rtmp: SocketAddr,
    http: SocketAddr,
    registry: Arc<Registry>,
}

async fn start(conf: &str) -> Running {
    let server = Server::bind(Config::parse(conf, "test").unwrap()).await.unwrap();
    let r = Running { rtmp: server.local_addrs()[0], http: server.http_addrs()[0], registry: server.registry.clone() };
    tokio::spawn(server.run());
    r
}

/// GET `path`: (status, content type, body).
async fn get(addr: SocketAddr, path: &str) -> (u16, String, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf)).await.unwrap().unwrap();
    let text = String::from_utf8(buf).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let ctype = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Type: "))
        .unwrap_or_default()
        .to_string();
    let len: usize = head.lines().find_map(|l| l.strip_prefix("Content-Length: ")).unwrap().parse().unwrap();
    assert_eq!(body.len(), len, "Content-Length");
    (status, ctype, body.to_string())
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

/// Read until the server closes the connection.
async fn closed(c: &mut Client) -> bool {
    while c.recv().await.is_some() {}
    true
}

fn players(reg: &Registry, app: &str, name: &str) -> Option<usize> {
    reg.list().into_iter().find(|(k, _, _)| *k == format!("0/{app}/{name}")).map(|(_, _, n)| n)
}

fn conf(dir: &Path) -> String {
    format!(
        r#"
rtmp {{
    server {{
        listen 127.0.0.1:0;
        application live {{
            live on;
            record all manual;
            record_path "{rec}";
        }}
        application idle {{ live off; }}
    }}
}}
http {{
    server {{
        listen 127.0.0.1:0;
        location /stat {{
            rtmp_stat all;
            rtmp_stat_stylesheet stat.xsl;
        }}
        location /stat.xsl {{ root "{dir}"; }}
        location /control {{ rtmp_control all; }}
        location /secret {{
            rtmp_control all;
            deny 127.0.0.1;
        }}
        location = /exact {{ rtmp_stat global; }}
    }}
    server {{
        listen 127.0.0.1:0;
        location / {{ root html; }}
    }}
}}
"#,
        rec = slash(&dir.join("rec")),
        dir = slash(dir),
    )
}

#[tokio::test]
async fn stat_page() {
    let d = tmp("page");
    std::fs::write(d.join("stat.xsl"), "<xsl/>").unwrap();
    let s = start(&conf(&d)).await;

    let mut publ = Client::ready(s.rtmp, "live").await;
    publ.publish("cam").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    let mut play = Client::ready(s.rtmp, "live").await;
    play.play("cam").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    publ.send(av(MSG_VIDEO, 0, AVC_HDR)).await;
    publ.send(av(MSG_AUDIO, 0, AAC_HDR)).await;
    publ.send(av(MSG_VIDEO, 40, KEY)).await;
    play.expect_av().await;

    let (status, ctype, xml) = get(s.http, "/stat").await;
    assert_eq!((status, ctype.as_str()), (200, "text/xml"));
    for want in [
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\r\n",
        "<?xml-stylesheet type=\"text/xsl\" href=\"stat.xsl\" ?>",
        "<nginx_rtmp_version>1.1.4</nginx_rtmp_version>",
        "<naccepted>2</naccepted>",
        "<server>\r\n<application>\r\n<name>live</name>\r\n<live>\r\n<stream>\r\n<name>cam</name>",
        "<application>\r\n<name>idle</name>\r\n</application>",
        "<address>127.0.0.1</address>",
        "<publishing/><active/></client>",
        "<video><width>640</width><height>360</height><frame_rate>0</frame_rate><codec>H264</codec>\
         <profile>High</profile><compat>0</compat><level>3.0</level></video>",
        "<audio><codec>AAC</codec><profile>LC</profile><channels>2</channels><sample_rate>44100</sample_rate></audio>",
        "<nclients>2</nclients>\r\n<publishing/>\r\n<active/>\r\n</stream>",
    ] {
        assert!(xml.contains(want), "missing {want:?} in\n{xml}");
    }
    assert_eq!(xml.matches("<client>").count(), 2);
    assert!(!xml.contains("<flashver>"), "the test client sends no flashVer");

    // The stylesheet comes from `root`; other locations of the server 404.
    assert_eq!(get(s.http, "/stat.xsl").await, (200, "text/xsl".into(), "<xsl/>".into()));
    assert_eq!(get(s.http, "/nothing").await.0, 404);
    assert_eq!(get(s.http, "/stat.xsl/../x").await.0, 400);
    // `location =` matches only the exact URI.
    assert!(get(s.http, "/exact").await.2.contains("<naccepted>"));
    assert_eq!(get(s.http, "/exactly").await.0, 404);
}

#[tokio::test]
async fn http_servers_without_rtmp_are_not_started() {
    let d = tmp("servers");
    let server = Server::bind(Config::parse(&conf(&d), "test").unwrap()).await.unwrap();
    assert_eq!(server.http_addrs().len(), 1);
}

#[tokio::test]
async fn control_record() {
    let d = tmp("record");
    let s = start(&conf(&d)).await;

    // Nothing published yet: no content.
    assert_eq!(get(s.http, "/control/record/start?app=live&name=cam").await.0, 204);

    let mut publ = Client::ready(s.rtmp, "live").await;
    publ.publish("cam").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    publ.send(av(MSG_VIDEO, 0, AVC_HDR)).await;
    publ.send(av(MSG_AUDIO, 0, AAC_HDR)).await;
    publ.send(av(MSG_VIDEO, 40, KEY)).await;

    let file = d.join("rec").join("cam.flv");
    let (status, _, body) = get(s.http, "/control/record/start?app=live&name=cam").await;
    assert_eq!(status, 200);
    assert_eq!(Path::new(&body), file);
    // Already recording.
    assert_eq!(get(s.http, "/control/record/start?app=live&name=cam").await.0, 204);
    assert_eq!(get(s.http, "/control/record/start?app=live&name=cam&rec=nope").await.0, 500);

    // A manual recorder starts at the next key frame.
    publ.send(av(MSG_VIDEO, 80, INTER)).await;
    publ.send(av(MSG_VIDEO, 120, KEY)).await;
    publ.send(av(MSG_AUDIO, 130, AUDIO)).await;
    let (status, _, body) = get(s.http, "/control/record/stop?app=live&name=cam").await;
    assert_eq!((status, Path::new(&body)), (200, file.as_path()));
    assert_eq!(get(s.http, "/control/record/stop?app=live&name=cam").await.0, 204);

    let data = std::fs::read(&file).unwrap();
    assert_eq!(&data[..3], b"FLV");
    // AAC header, AVC header (nginx writes them in this order), key frame, audio
    let mut tags = Vec::new();
    let mut p = 13;
    while p < data.len() {
        let len = u32::from_be_bytes([0, data[p + 1], data[p + 2], data[p + 3]]) as usize;
        tags.push((data[p], data[p + 11..p + 11 + len].to_vec()));
        p += 15 + len;
    }
    let want: Vec<(u8, Vec<u8>)> =
        [(MSG_AUDIO, AAC_HDR), (MSG_VIDEO, AVC_HDR), (MSG_VIDEO, KEY), (MSG_AUDIO, AUDIO)]
            .iter()
            .map(|(t, b)| (*t, b.to_vec()))
            .collect();
    assert_eq!(tags, want);
}

#[tokio::test]
async fn control_drop_and_redirect() {
    let d = tmp("drop");
    let s = start(&conf(&d)).await;

    let mut publ = Client::ready(s.rtmp, "live").await;
    publ.publish("cam").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    let mut other = Client::ready(s.rtmp, "live").await;
    other.publish("cam2").await;
    assert_eq!(other.expect_status().await, "NetStream.Publish.Start");
    let mut play = Client::ready(s.rtmp, "live").await;
    play.play("cam").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");

    // Argument errors.
    assert_eq!(get(s.http, "/control/drop/everyone").await.0, 500);
    assert_eq!(get(s.http, "/control/drop/client?srv=3").await.0, 500);
    assert_eq!(get(s.http, "/control/redirect/client").await.0, 500, "no newname");
    assert_eq!(get(s.http, "/control/unknown/client").await.0, 404);
    assert_eq!(get(s.http, "/secret/drop/client").await.0, 403);
    assert_eq!(get(s.http, "/control/drop/client?addr=10.0.0.1").await.2, "0");

    // The player moves to cam2.
    let (status, _, body) = get(s.http, "/control/redirect/subscriber?app=live&name=cam&newname=cam2").await;
    assert_eq!((status, body.as_str()), (200, "1"));
    assert_eq!(play.expect_status().await, "NetStream.Play.Stop");
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    assert_eq!(players(&s.registry, "live", "cam2"), Some(1));
    assert_eq!(players(&s.registry, "live", "cam"), Some(0));

    // Drop the publisher of cam only.
    let (status, _, body) = get(s.http, "/control/drop/publisher?app=live&name=cam").await;
    assert_eq!((status, body.as_str()), (200, "1"));
    assert!(closed(&mut publ).await);
    wait_until("cam removed", || players(&s.registry, "live", "cam").is_none().then_some(())).await;

    // Everyone else, by client id and then by filter.
    let ids: Vec<u64> = s.registry.streams().iter().flat_map(|st| st.players.clone()).collect();
    assert_eq!(ids.len(), 1);
    let (_, _, body) = get(s.http, &format!("/control/drop/client?clientid={}", ids[0])).await;
    assert_eq!(body, "1");
    assert!(closed(&mut play).await);
    let (_, _, body) = get(s.http, "/control/drop/client?app=live").await;
    assert_eq!(body, "1");
    assert!(closed(&mut other).await);
}
