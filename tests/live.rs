//! End-to-end: publisher -> server -> player over real TCP sockets.

mod common;

use common::{Client, av};
use rtmp_server::amf::Amf0;
use rtmp_server::config::Config;
use rtmp_server::message::{self as msg, *};
use rtmp_server::server::Server;
use std::net::SocketAddr;
use std::time::Duration;

const CONF: &str = r#"
rtmp {
    server {
        listen 127.0.0.1:0;
        chunk_size 4096;
        application live {
            live on;
            meta copy;
            allow publish all;
        }
        application closed {
            live on;
            deny publish all;
        }
    }
}
"#;

async fn start() -> SocketAddr {
    let server = Server::bind(Config::parse(CONF, "test").unwrap()).await.unwrap();
    let addr = server.local_addrs()[0];
    tokio::spawn(server.run());
    addr
}

#[tokio::test]
async fn publish_and_play() {
    let addr = start().await;

    // --- publisher
    let mut publ = Client::connect(addr, "live").await;
    let r = publ.expect_amf("_result").await;
    assert_eq!(r[1], Amf0::Number(1.0));
    assert_eq!(r[3].get("code").and_then(Amf0::as_str), Some("NetConnection.Connect.Success"));
    assert_eq!(r[2].get("fmsVer").and_then(Amf0::as_str), Some("FMS/3,0,1,123"));
    publ.send(msg::chunk_size(60_000)).await;
    publ.create_stream().await;
    publ.send(amf_cmd(8, 1, &[Amf0::str("publish"), Amf0::Number(3.0), Amf0::Null, Amf0::str("test?key=1"), Amf0::str("live")])).await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");

    // metadata + sequence headers before any player joins
    publ.send(amf_data(4, 1, &[
        Amf0::str("@setDataFrame"),
        Amf0::str("onMetaData"),
        Amf0::EcmaArray(vec![("width".into(), Amf0::Number(640.0))]),
    ])).await;
    publ.send(av(MSG_VIDEO, 0, &[0x17, 0x00, 0, 0, 0, 0xAA])).await; // AVC seq header
    publ.send(av(MSG_AUDIO, 0, &[0xAF, 0x00, 0x12, 0x10])).await; // AAC seq header
    publ.send(av(MSG_VIDEO, 10, &[0x27, 0x01, 0, 0, 0, 1])).await; // inter frame

    // --- second publisher on the same name is rejected
    let mut dup = Client::connect(addr, "live").await;
    dup.expect_amf("_result").await;
    dup.create_stream().await;
    dup.send(amf_cmd(8, 1, &[Amf0::str("publish"), Amf0::Number(3.0), Amf0::Null, Amf0::str("test"), Amf0::str("live")])).await;
    assert_eq!(dup.expect_status().await, "NetStream.Publish.BadName");

    // --- player
    let mut play = Client::connect(addr, "live").await;
    play.expect_amf("_result").await;
    play.create_stream().await;
    play.send(amf_cmd(8, 1, &[Amf0::str("play"), Amf0::Number(4.0), Amf0::Null, Amf0::str("test")])).await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    play.expect_amf("|RtmpSampleAccess").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // An inter frame must be skipped (wait_key on), then the key frame
    // arrives preceded by metadata and the cached AVC header.
    publ.send(av(MSG_VIDEO, 20, &[0x27, 0x01, 0, 0, 0, 2])).await;
    publ.send(av(MSG_VIDEO, 40, &[0x17, 0x01, 0, 0, 0, 3])).await;
    publ.send(av(MSG_AUDIO, 41, &[0xAF, 0x01, 0x55])).await;

    let meta = play.expect_amf("onMetaData").await;
    assert_eq!(meta[1].get("width"), Some(&Amf0::Number(640.0)));
    let h = play.expect_av().await;
    assert_eq!(&h.payload[..], &[0x17, 0x00, 0, 0, 0, 0xAA], "AVC header first");
    let k = play.expect_av().await;
    assert_eq!(&k.payload[..], &[0x17, 0x01, 0, 0, 0, 3]);
    assert_eq!(k.timestamp, 40);
    let ah = play.expect_av().await;
    assert_eq!(&ah.payload[..], &[0xAF, 0x00, 0x12, 0x10], "AAC header before first audio");
    let a = play.expect_av().await;
    assert_eq!(&a.payload[..], &[0xAF, 0x01, 0x55]);
    assert_eq!(a.timestamp, 41);

    // Large frame crosses many chunks with the publisher's 60000 chunk size
    // and the server's 4096 outgoing chunk size.
    static BIG: [u8; 100_000] = {
        let mut b = [0x5Au8; 100_000];
        b[0] = 0x27;
        b[1] = 0x01;
        b
    };
    publ.send(av(MSG_VIDEO, 80, &BIG)).await;
    let big = play.expect_av().await;
    assert_eq!(big.payload.len(), BIG.len());
    assert_eq!(big.timestamp, 80);

    // Publisher leaves: player gets StreamEOF.
    publ.send(amf_cmd(8, 1, &[Amf0::str("deleteStream"), Amf0::Number(5.0), Amf0::Null, Amf0::Number(1.0)])).await;
    assert_eq!(publ.expect_status().await, "NetStream.Unpublish.Success");
    loop {
        let m = play.recv().await.expect("player closed");
        if m.type_id == MSG_USER && m.payload[..2] == USER_STREAM_EOF.to_be_bytes() {
            break;
        }
    }
}

#[tokio::test]
async fn unknown_app_and_access_denied_close() {
    let addr = start().await;
    let mut c = Client::connect(addr, "nope").await;
    assert!(c.recv().await.is_none(), "unknown app must close the connection");

    let mut c = Client::connect(addr, "closed").await;
    c.expect_amf("_result").await;
    c.create_stream().await;
    c.send(amf_cmd(8, 1, &[Amf0::str("publish"), Amf0::Number(3.0), Amf0::Null, Amf0::str("x"), Amf0::str("live")])).await;
    while let Some(m) = c.recv().await {
        assert_ne!(m.type_id, MSG_AMF_CMD, "no status expected on access deny");
    }
}
