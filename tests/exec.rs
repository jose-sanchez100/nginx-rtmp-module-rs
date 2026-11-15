//! exec module: real child processes started on stream events. The child
//! is the platform's `echo`, appending a line to a file.

mod common;

use common::Client;
use nginx_rtmp::config::Config;
use nginx_rtmp::server::Server;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(windows)]
const ECHO: &str = "cmd /c echo";
#[cfg(not(windows))]
const ECHO: &str = "echo";

/// A fresh directory for one test, as a config-friendly string.
fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rtmp-exec-{name}-{}", std::process::id()));
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

fn lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p).unwrap_or_default().lines().map(|l| l.trim().to_string()).collect()
}

/// Wait until `p` has at least `n` lines.
async fn wait_lines(p: &Path, n: usize) -> Vec<String> {
    let t = Instant::now();
    loop {
        let l = lines(p);
        if l.len() >= n {
            return l;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "{}: {l:?}, expected {n} lines", p.display());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn event_commands() {
    let d = tmp("events");
    let log = d.join("events.log");
    let out = format!("\">>{}\"", slash(&log));
    let addr = start(&format!(
        r#"rtmp {{ server {{ listen 127.0.0.1:0; application live {{
            live on;
            exec_publish {ECHO} publish $app $name $args $addr {out};
            exec_publish_done {ECHO} publish_done $name {out};
            exec_play {ECHO} play $name {out};
            exec_play_done {ECHO} play_done $name {out};
        }} }} }}"#
    ))
    .await;

    let mut publ = Client::ready(addr, "live").await;
    publ.publish("cam?k=1").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    assert_eq!(wait_lines(&log, 1).await, ["publish live cam k=1 127.0.0.1"]);

    let mut play = Client::ready(addr, "live").await;
    play.play("cam").await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    wait_lines(&log, 2).await;
    drop(play);
    wait_lines(&log, 3).await;
    drop(publ);
    let l = wait_lines(&log, 4).await;
    assert_eq!(l[1..], ["play cam", "play_done cam", "publish_done cam"]);
}

#[tokio::test]
async fn push_respawns_until_unpublished() {
    let d = tmp("push");
    let log = d.join("push.log");
    let addr = start(&format!(
        r#"rtmp {{ respawn_timeout 100ms; server {{ listen 127.0.0.1:0; application live {{
            live on;
            exec {ECHO} push $name ">>{}";
        }} }} }}"#,
        slash(&log)
    ))
    .await;

    let mut publ = Client::ready(addr, "live").await;
    publ.publish("s").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    // The child exits at once and is respawned.
    let l = wait_lines(&log, 3).await;
    assert!(l.iter().all(|l| l == "push s"), "{l:?}");
    drop(publ);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let n = lines(&log).len();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(lines(&log).len(), n, "no respawn after the publisher left");
}

#[tokio::test]
async fn respawn_off_and_name_filter() {
    let d = tmp("filter");
    let log = d.join("push.log");
    let addr = start(&format!(
        r#"rtmp {{ respawn_timeout 100ms; server {{ listen 127.0.0.1:0; application live {{
            live on;
            respawn off;
            exec_options on;
            exec {ECHO} push $name name=wanted ">>{}";
        }} }} }}"#,
        slash(&log)
    ))
    .await;

    let mut other = Client::ready(addr, "live").await;
    other.publish("other").await;
    assert_eq!(other.expect_status().await, "NetStream.Publish.Start");
    let mut publ = Client::ready(addr, "live").await;
    publ.publish("wanted").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    assert_eq!(wait_lines(&log, 1).await, ["push wanted"]);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(lines(&log), ["push wanted"], "respawn off: ran once; filtered stream: never");
}

#[tokio::test]
async fn pull_runs_once_per_stream() {
    let d = tmp("pull");
    let log = d.join("pull.log");
    let addr = start(&format!(
        r#"rtmp {{ server {{ listen 127.0.0.1:0; application src {{
            live on;
            respawn off;
            exec_pull {ECHO} pull $app $name ">>{}";
        }} }} }}"#,
        slash(&log)
    ))
    .await;

    let mut p1 = Client::ready(addr, "src").await;
    p1.play("cam").await;
    assert_eq!(p1.expect_status().await, "NetStream.Play.Start");
    let mut p2 = Client::ready(addr, "src").await;
    p2.play("cam").await;
    assert_eq!(p2.expect_status().await, "NetStream.Play.Start");
    assert_eq!(wait_lines(&log, 1).await, ["pull src cam"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lines(&log).len(), 1, "second player shares the running pull");

    drop((p1, p2));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut p3 = Client::ready(addr, "src").await;
    p3.play("cam").await;
    assert_eq!(p3.expect_status().await, "NetStream.Play.Start");
    assert_eq!(wait_lines(&log, 2).await, ["pull src cam", "pull src cam"]);
}

#[tokio::test]
async fn static_starts_with_the_server() {
    let d = tmp("static");
    let log = d.join("static.log");
    start(&format!(
        r#"rtmp {{ respawn_timeout 1m; exec_static {ECHO} static $name ">>{}";
            server {{ listen 127.0.0.1:0; application live {{ live on; }} }} }}"#,
        slash(&log)
    ))
    .await;
    // Not evaluated: there is no session.
    assert_eq!(wait_lines(&log, 1).await, ["static $name"]);
}

/// The child's stderr goes where `2>>` says, relative paths under the
/// prefix, and a missing program is reported without crashing.
#[tokio::test]
async fn redirects_and_prefix() {
    let d = tmp("prefix");
    let mut config = Config::parse(
        &format!(
            r#"rtmp {{ respawn_timeout 1m; server {{ listen 127.0.0.1:0; application live {{
                live on;
                exec_publish {ECHO} hello $name >logs/out.log;
                exec_publish no-such-program-xyz;
            }} }} }}"#
        ),
        "test",
    )
    .unwrap();
    config.prefix = Some(d.clone());
    let server = Server::bind(config).await.unwrap();
    let addr = server.local_addrs()[0];
    tokio::spawn(server.run());

    let mut publ = Client::ready(addr, "live").await;
    publ.publish("x").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    assert_eq!(wait_lines(&d.join("logs/out.log"), 1).await, ["hello x"]);
}
