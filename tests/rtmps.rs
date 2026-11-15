//! RTMPS: `listen ... ssl` on the server side, `rtmps://` relays on the
//! client side.

mod common;

use common::{Client, av};
use nginx_rtmp::amf::Amf0;
use nginx_rtmp::config::Config;
use nginx_rtmp::message::*;
use nginx_rtmp::server::Server;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data");

/// TLS client that trusts the self-signed test certificate.
async fn tls_client(addr: std::net::SocketAddr, app: &str) -> Client {
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(format!("{DATA}/cert.pem")).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let sock = TcpStream::connect(addr).await.unwrap();
    let tls = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), sock)
        .await
        .expect("TLS handshake");
    Client::connect_io(Box::new(tls), &format!("rtmps://localhost:{}/{app}", addr.port()), app).await
}

#[tokio::test]
async fn publish_over_tls_play_over_tcp() {
    let conf = format!(
        r#"
        rtmp {{
            server {{
                listen 127.0.0.1:0;
                listen 127.0.0.1:0 ssl;
                ssl_certificate {DATA}/cert.pem;
                ssl_certificate_key {DATA}/key.pem;
                application live {{ live on; }}
            }}
        }}"#
    );
    // Both listeners use port 0, so they are distinct sockets.
    let server = Server::bind(Config::parse(&conf, "test").unwrap()).await.unwrap();
    let info = server.listen_info();
    let plain = info.iter().find(|i| !i.2).unwrap().1;
    let secure = info.iter().find(|i| i.2).unwrap().1;
    tokio::spawn(server.run());

    let mut publ = tls_client(secure, "live").await;
    publ.expect_amf("_result").await;
    publ.create_stream().await;
    publ.send(amf_cmd(8, 1, &[Amf0::str("publish"), Amf0::Number(3.0), Amf0::Null, Amf0::str("s"), Amf0::str("live")])).await;
    assert_eq!(publ.expect_status().await, "NetStream.Publish.Start");

    let mut play = Client::connect(plain, "live").await;
    play.expect_amf("_result").await;
    play.create_stream().await;
    play.send(amf_cmd(8, 1, &[Amf0::str("play"), Amf0::Number(4.0), Amf0::Null, Amf0::str("s")])).await;
    assert_eq!(play.expect_status().await, "NetStream.Play.Start");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    publ.send(av(MSG_VIDEO, 0, &[0x17, 0x00, 0, 0, 0, 0xAA])).await;
    publ.send(av(MSG_VIDEO, 40, &[0x17, 0x01, 0, 0, 0, 7])).await;
    let h = play.expect_av().await;
    assert_eq!(&h.payload[..], &[0x17, 0x00, 0, 0, 0, 0xAA]);
    let k = play.expect_av().await;
    assert_eq!(&k.payload[..], &[0x17, 0x01, 0, 0, 0, 7]);
}

#[tokio::test]
async fn plain_client_on_tls_port_is_dropped() {
    let conf = format!(
        "rtmp {{ server {{ listen 127.0.0.1:0 ssl; timeout 2s; ssl_certificate {DATA}/cert.pem; ssl_certificate_key {DATA}/key.pem; application live {{ live on; }} }} }}"
    );
    let server = Server::bind(Config::parse(&conf, "test").unwrap()).await.unwrap();
    let addr = server.local_addrs()[0];
    tokio::spawn(server.run());
    let mut sock = TcpStream::connect(addr).await.unwrap();
    let r = tokio::time::timeout(std::time::Duration::from_secs(5), nginx_rtmp::handshake::client(&mut sock, 0)).await;
    assert!(matches!(r, Ok(Err(_))), "an RTMP handshake must not succeed on an RTMPS port");
}

#[test]
fn ssl_listen_needs_certificate() {
    let e = Config::parse("rtmp { server { listen 1936 ssl; } }", "t").unwrap_err().to_string();
    assert!(e.contains("ssl_certificate"), "{e}");
    let e = Config::parse("rtmp { server { listen 1936 ssl; ssl_certificate a.pem; } }", "t").unwrap_err().to_string();
    assert!(e.contains("ssl_certificate_key"), "{e}");
    // rtmp{}-level certificate is inherited.
    let c = Config::parse("rtmp { ssl_certificate a.pem; ssl_certificate_key b.pem; server { listen 1935; listen 1936 ssl; } }", "t").unwrap();
    assert_eq!(c.servers[0].listen_ssl, vec![false, true]);
    assert_eq!(c.servers[0].listen.len(), 2);
}

#[test]
fn rtmps_urls() {
    use nginx_rtmp::config::RelayTarget;
    let t = RelayTarget::parse_url("rtmps://live-api-s.facebook.com:443/rtmp/KEY").unwrap();
    assert!(t.tls);
    assert_eq!((t.host.as_str(), t.port, t.app.as_str(), t.play_path.as_str()), ("live-api-s.facebook.com", 443, "rtmp", "KEY"));
    let t = RelayTarget::parse_url("RTMPS://a.rtmps.youtube.com/live2").unwrap();
    assert!(t.tls && t.port == 443);
    let t = RelayTarget::parse_url("rtmp://a.rtmp.youtube.com/live2").unwrap();
    assert!(!t.tls && t.port == 1935);
}

/// Real services: TLS + RTMP handshake + connect against Facebook and
/// YouTube ingest. Needs the internet: `cargo test --test rtmps -- --ignored`.
#[tokio::test]
#[ignore]
async fn real_ingest_servers() {
    for (host, app) in [("live-api-s.facebook.com", "rtmp"), ("a.rtmps.youtube.com", "live2")] {
        let sock = nginx_rtmp::tls::connect(host, 443, true).await.unwrap_or_else(|e| panic!("{host}: {e}"));
        let mut c = Client::connect_io(sock, &format!("rtmps://{host}:443/{app}"), app).await;
        let r = c.expect_amf("_result").await;
        println!("{host}: {:?}", r[3].get("code"));
    }
}
