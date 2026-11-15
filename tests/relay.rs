//! push / pull / static pull between two server instances.

mod common;

use common::{Client, av};
use nginx_rtmp::config::Config;
use nginx_rtmp::live::Registry;
use nginx_rtmp::message::*;
use nginx_rtmp::server::Server;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};

const AVC_HDR: &[u8] = &[0x17, 0x00, 0, 0, 0, 0xAA];
const AAC_HDR: &[u8] = &[0xAF, 0x00, 0x12, 0x10];
const KEY: &[u8] = &[0x17, 0x01, 0, 0, 0, 7];

async fn start(conf: &str) -> (SocketAddr, Arc<Registry>) {
    let server = Server::bind(Config::parse(conf, "test").unwrap()).await.unwrap();
    let addr = server.local_addrs()[0];
    let reg = server.registry.clone();
    tokio::spawn(server.run());
    (addr, reg)
}

async fn origin() -> (SocketAddr, Arc<Registry>) {
    start("rtmp { server { listen 127.0.0.1:0; application origin { live on; } } }").await
}

/// (publisher present, player count) of a stream, if it exists.
fn stream_state(reg: &Registry, key: &str) -> Option<(bool, usize)> {
    reg.list().into_iter().find(|(k, _, _)| k == key).map(|(_, p, n)| (p, n))
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    timeout(Duration::from_secs(10), async {
        while !cond() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Read media until a key frame arrives; returns (got AVC header first, key ts).
async fn until_key(c: &mut Client) -> (bool, u32) {
    let mut saw_header = false;
    loop {
        let m = c.expect_av().await;
        if &m.payload[..] == AVC_HDR {
            saw_header = true;
        }
        if m.payload.len() > 1 && m.payload[0] == 0x17 && m.payload[1] == 1 {
            return (saw_header, m.timestamp);
        }
    }
}

#[tokio::test]
async fn push_to_upstream() {
    let (up, up_reg) = origin().await;
    let (edge, _) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on;
            push rtmp://{up}/origin;
            push rtmp://{up}/origin/renamed name=only_this;   # filtered out
            push_reconnect 200ms;
        }} }} }}"
    ))
    .await;

    let mut publ = Client::ready(edge, "live").await;
    publ.publish("cam1").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    publ.send(av(MSG_VIDEO, 0, AVC_HDR)).await;
    publ.send(av(MSG_AUDIO, 0, AAC_HDR)).await;

    // The push shows up as a publisher on the origin.
    wait_for("push to publish on origin", || stream_state(&up_reg, "0/origin/cam1") == Some((true, 0))).await;
    assert!(stream_state(&up_reg, "0/origin/renamed").is_none(), "name= filter must skip the second push");

    let mut play = Client::ready(up, "origin").await;
    play.play("cam1").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    wait_for("player on origin", || stream_state(&up_reg, "0/origin/cam1") == Some((true, 1))).await;

    // The edge->origin push joins the edge stream as a player, so it waits
    // for a key frame and leads with the cached headers.
    for i in 1..=3u32 {
        publ.send(av(MSG_VIDEO, i * 40, KEY)).await;
    }
    let (hdr, ts) = until_key(&mut play).await;
    assert!(hdr, "AVC header must precede the first key frame");
    assert_eq!(ts, 40);

    // Publisher leaves the edge: the push stops and the origin stream loses
    // its publisher; the origin player gets StreamEOF.
    drop(publ);
    loop {
        let m = play.recv().await.expect("origin player closed");
        if m.type_id == MSG_USER && m.payload[..2] == USER_STREAM_EOF.to_be_bytes() {
            break;
        }
    }
    wait_for("push to stop", || stream_state(&up_reg, "0/origin/cam1") == Some((false, 1))).await;
}

#[tokio::test]
async fn push_reconnects_while_publishing() {
    // Reserve a port, start the edge pointing at it, and only then bring the
    // origin up there: the push must keep retrying until it connects.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let (edge, _) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on; push rtmp://127.0.0.1:{port}/origin; push_reconnect 100ms;
        }} }} }}"
    ))
    .await;
    let mut publ = Client::ready(edge, "live").await;
    publ.publish("late").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    sleep(Duration::from_millis(300)).await;

    let (_, up_reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:{port}; application origin {{ live on; }} }} }}"
    ))
    .await;
    wait_for("push reconnect", || stream_state(&up_reg, "0/origin/late").is_some_and(|s| s.0)).await;
}

#[tokio::test]
async fn pull_on_demand() {
    let (up, up_reg) = origin().await;
    let (edge, edge_reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application edge {{
            live on;
            pull rtmp://{up}/origin;
        }} }} }}"
    ))
    .await;

    let mut publ = Client::ready(up, "origin").await;
    publ.publish("cam").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    publ.send(av(MSG_VIDEO, 0, AVC_HDR)).await;

    // Nobody is pulling yet.
    sleep(Duration::from_millis(200)).await;
    assert_eq!(stream_state(&up_reg, "0/origin/cam"), Some((true, 0)));

    let mut play = Client::ready(edge, "edge").await;
    play.play("cam").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    wait_for("pull to play from origin", || stream_state(&up_reg, "0/origin/cam") == Some((true, 1))).await;
    wait_for("pull to publish on edge", || stream_state(&edge_reg, "0/edge/cam") == Some((true, 1))).await;

    let feeder = tokio::spawn(async move {
        for i in 1..200u32 {
            publ.send(av(MSG_VIDEO, i * 40, KEY)).await;
            sleep(Duration::from_millis(20)).await;
        }
        publ
    });
    let (hdr, _) = until_key(&mut play).await;
    assert!(hdr, "edge player must get the AVC header pulled from origin");

    // Last edge player leaves: the pull disconnects from the origin.
    drop(play);
    wait_for("pull to close", || stream_state(&up_reg, "0/origin/cam") == Some((true, 0))).await;
    wait_for("edge stream to go away", || stream_state(&edge_reg, "0/edge/cam").is_none()).await;
    feeder.abort();
}

#[tokio::test]
async fn pull_target_unreachable_disconnects_player() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let (edge, _) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application edge {{ live on; pull rtmp://127.0.0.1:{port}/origin; }} }} }}"
    ))
    .await;
    let mut play = Client::ready(edge, "edge").await;
    play.play("nothing").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    // Pull fails -> orphaned player is disconnected.
    while play.recv().await.is_some() {}
}

#[tokio::test]
async fn static_pull() {
    let (up, up_reg) = origin().await;
    let (_edge, edge_reg) = start(&format!(
        "rtmp {{ server {{ listen 127.0.0.1:0; application mirror {{
            live on;
            pull rtmp://{up}/origin/src name=copy static;
            pull_reconnect 100ms;
        }} }} }}"
    ))
    .await;

    // With no players anywhere, the static pull still plays from the origin
    // and publishes locally.
    wait_for("static pull on origin", || stream_state(&up_reg, "0/origin/src") == Some((false, 1))).await;
    wait_for("static pull publishing on edge", || stream_state(&edge_reg, "0/mirror/copy") == Some((true, 0))).await;
    sleep(Duration::from_millis(1500)).await;
    assert_eq!(stream_state(&edge_reg, "0/mirror/copy"), Some((true, 0)), "static pull must not close without players");
}
