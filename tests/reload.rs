//! Configuration reload (keeping sessions) and the configuration editor.

mod common;

use common::{Client, av};
use nginx_rtmp::config::Config;
use nginx_rtmp::editor;
use nginx_rtmp::message::*;
use nginx_rtmp::server::Server;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn conf(apps: &[&str], extra_listen: bool) -> Config {
    let apps: String = apps.iter().map(|a| format!("application {a} {{ live on; }}\n")).collect();
    let listen = if extra_listen { "listen 127.0.0.1:0;\nlisten 127.0.0.1:0;" } else { "listen 127.0.0.1:0;" };
    Config::parse(&format!("rtmp {{ server {{ {listen}\n{apps} }} }}"), "test").unwrap()
}

/// Whether a client can `connect` to `app` (unknown applications are
/// closed without an answer).
async fn connects(addr: SocketAddr, app: &str) -> bool {
    let mut c = Client::connect(addr, app).await;
    let r = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c.recv().await {
                None => return false,
                Some(m) if m.type_id == MSG_AMF_CMD => return true,
                Some(_) => {}
            }
        }
    })
    .await;
    r.unwrap_or(false)
}

#[tokio::test]
async fn reload_keeps_sessions() {
    let server = Server::bind(conf(&["live"], false)).await.unwrap();
    let addr = server.local_addrs()[0];
    let registry = server.registry.clone();
    let reloader = server.reloader();
    let listen = server.listen_info_shared();
    tokio::spawn(server.run());

    let mut publ = Client::ready(addr, "live").await;
    publ.publish("cam").await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");
    assert!(!connects(addr, "other").await);

    // A new application and a second listener.
    reloader.reload(conf(&["live", "other"], true)).await.unwrap();
    assert_eq!(listen.read().unwrap().len(), 2);
    assert_eq!(listen.read().unwrap()[0].1, addr, "the existing listener is kept");
    assert!(connects(addr, "other").await, "new connections use the new configuration");
    let second = listen.read().unwrap()[1].1;
    assert!(connects(second, "live").await);

    // The publisher was not disturbed: it still publishes to its players.
    let mut player = Client::ready(addr, "live").await;
    player.play("cam").await;
    while player.expect_status().await != "NetStream.Play.Start" {}
    publ.send(av(MSG_VIDEO, 0, &[0x17, 0x01, 0, 0, 0, 7])).await;
    let m = tokio::time::timeout(Duration::from_secs(5), player.expect_av()).await.unwrap();
    assert_eq!(m.type_id, MSG_VIDEO);
    assert!(registry.streams().iter().any(|s| s.name == "cam" && s.publisher.is_some()));

    // Back to one listener: the second one is closed.
    reloader.reload(conf(&["live"], false)).await.unwrap();
    assert_eq!(listen.read().unwrap().len(), 1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(tokio::net::TcpStream::connect(second).await.is_err());
}

#[tokio::test]
async fn failed_reload_keeps_the_running_configuration() {
    let server = Server::bind(conf(&["live"], false)).await.unwrap();
    let addr = server.local_addrs()[0];
    let reloader = server.reloader();
    tokio::spawn(server.run());

    // A port that is taken: nothing changes.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let bad = Config::parse(
        &format!("rtmp {{ server {{ listen 127.0.0.1:0; listen 127.0.0.1:{port}; application other {{ live on; }} }} }}"),
        "test",
    )
    .unwrap();
    let e = reloader.reload(bad).await.unwrap_err();
    assert!(e.contains("bind"), "{e}");
    assert!(connects(addr, "live").await);
    assert!(!connects(addr, "other").await);
}

/// HTTP request to the editor: (status, JSON body).
async fn call(addr: SocketAddr, method: &str, path: &str, host: &str, body: Option<Value>) -> (u16, Value) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    s.write_all(
        format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf)).await.unwrap().unwrap();
    let text = String::from_utf8(buf).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::String(body.to_string())))
}

#[tokio::test]
async fn editor_saves_and_reloads() {
    let dir = std::env::temp_dir().join(format!("rtmp-editor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nginx.conf");
    let original = "# my server\nrtmp {\n    server {\n        listen 127.0.0.1:0;\n\n        application live {\n            live on; # keep\n        }\n    }\n}\n";
    std::fs::write(&path, original).unwrap();

    let server = Server::bind(Config::load(&path).unwrap()).await.unwrap();
    let addr = server.local_addrs()[0];
    let reloader = server.reloader();
    tokio::spawn(server.run());
    let p = path.clone();
    let apply: editor::Apply = Arc::new(move || {
        let (reloader, p) = (reloader.clone(), p.clone());
        Box::pin(async move {
            let c = Config::load(&p).map_err(|e| e.to_string())?;
            reloader.reload(c).await.map(|()| "reloaded".to_string())
        })
    });
    let ed = editor::start(path.clone(), 0, apply).await.unwrap();
    let host = ed.addr.to_string();
    let base = ed.url.trim_start_matches(&format!("http://{host}")).to_string();

    // The token and a loopback Host are required.
    assert_eq!(call(ed.addr, "GET", "/api/load", &host, None).await.0, 404);
    assert_eq!(call(ed.addr, "GET", &format!("{base}api/load"), "evil.example", None).await.0, 403);
    let (st, page) = call(ed.addr, "GET", &base, &host, None).await;
    assert_eq!(st, 200);
    assert!(page.as_str().unwrap().contains("nginx.conf editor"));

    let (st, schema) = call(ed.addr, "GET", &format!("{base}api/schema"), &host, None).await;
    assert_eq!(st, 200);
    assert!(schema.as_array().unwrap().iter().any(|s| s["name"] == "push"));

    let (st, loaded) = call(ed.addr, "GET", &format!("{base}api/load"), &host, None).await;
    assert_eq!(st, 200);
    assert_eq!(loaded["text"], original);
    assert_eq!(loaded["check"]["ok"], true);
    let mut tree = loaded["tree"].clone();

    // Add an application, as the page does.
    let server_block = &mut tree[1]["block"][0]["block"];
    server_block
        .as_array_mut()
        .unwrap()
        .push(json!({ "t": "dir", "name": "application", "args": ["other"], "block": [{ "t": "dir", "name": "live", "args": ["on"] }] }));
    let (_, rendered) = call(ed.addr, "POST", &format!("{base}api/render"), &host, Some(json!({ "tree": tree }))).await;
    assert_eq!(rendered["check"]["ok"], true, "{rendered}");

    // Help for the server block: recipes and a row for each directive.
    let (st, h) = call(ed.addr, "POST", &format!("{base}api/help"), &host, Some(json!({ "tree": tree, "path": [1, 0] }))).await;
    assert_eq!(st, 200, "{h}");
    assert!(h["recipes"].as_array().unwrap().iter().any(|r| r["name"] == "Live application"), "{h}");
    assert!(h["rows"]["0"]["problems"].as_array().unwrap().is_empty(), "{h}");
    // A bad field, an inherited setting and a note with a fix.
    let mut t2 = tree.clone();
    t2[1]["block"].as_array_mut().unwrap().insert(0, json!({ "t": "dir", "name": "live", "args": ["off"] }));
    let app = &mut t2[1]["block"][1]["block"][3]["block"];
    app.as_array_mut().unwrap().push(json!({ "t": "dir", "name": "record_path", "args": ["/tmp"] }));
    app.as_array_mut().unwrap().push(json!({ "t": "dir", "name": "drop_idle_publisher", "args": ["10x"] }));
    let (_, h) = call(ed.addr, "POST", &format!("{base}api/help"), &host, Some(json!({ "tree": t2, "path": [1, 1, 3] }))).await;
    assert_eq!(h["rows"]["2"]["problems"][0][0], 0, "{h}");
    assert_eq!(h["rows"]["0"]["overrides"], json!({ "args": ["off"], "from": "rtmp" }), "{h}");
    assert!(h["notes"].as_array().unwrap().iter().any(|n| n["fix"]["label"] == "Add record all"), "{h}");
    let (_, hs) = call(ed.addr, "POST", &format!("{base}api/help"), &host, Some(json!({ "tree": t2, "path": [1, 1] }))).await;
    assert!(hs["inherited"].as_array().unwrap().iter().any(|x| x["name"] == "live" && x["from"] == "rtmp"), "{hs}");
    // A recipe comes back as the new tree.
    let k = h["recipes"].as_array().unwrap().iter().find(|r| r["name"] == "Record to disk").unwrap()["id"].clone();
    let (st, r) =
        call(ed.addr, "POST", &format!("{base}api/recipe"), &host, Some(json!({ "tree": t2, "path": [1, 1, 3], "recipe": k }))).await;
    assert_eq!(st, 200, "{r}");
    assert!(r["tree"][1]["block"][1]["block"][3]["block"].as_array().unwrap().iter().any(|d| d["name"] == "record"), "{r}");

    // Invalid configurations are not saved.
    let (st, r) = call(ed.addr, "POST", &format!("{base}api/save"), &host, Some(json!({ "text": "rtmp { server { listen x; } }", "hash": loaded["hash"] }))).await;
    assert_eq!(st, 400, "{r}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

    let (st, r) =
        call(ed.addr, "POST", &format!("{base}api/save"), &host, Some(json!({ "tree": tree, "hash": loaded["hash"] }))).await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["reload"]["ok"], true, "{r}");
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.starts_with("# my server\n"), "comments are kept: {saved}");
    assert!(saved.contains("live on; # keep"), "{saved}");
    assert!(saved.contains("        application other {\n            live on;\n        }\n"), "{saved}");
    assert_eq!(std::fs::read_to_string(dir.join("nginx.conf.bak")).unwrap(), original);
    assert!(connects(addr, "other").await, "the server was reloaded");

    // A stale hash means the file changed meanwhile.
    let (st, _) =
        call(ed.addr, "POST", &format!("{base}api/save"), &host, Some(json!({ "tree": tree, "hash": loaded["hash"] }))).await;
    assert_eq!(st, 409);
    let _ = std::fs::remove_dir_all(&dir);
}
