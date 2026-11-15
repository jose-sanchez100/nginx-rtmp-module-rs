//! Minimal RTMP client used by the integration tests.
#![allow(dead_code)]

use bytes::{Bytes, BytesMut};
use rtmp_server::amf::{self, Amf0};
use rtmp_server::chunk::{ChunkDecoder, ChunkEncoder, Message};
use rtmp_server::handshake;
use rtmp_server::message::*;
use std::net::SocketAddr;
use std::time::Duration;
use rtmp_server::tls::BoxIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub struct Client {
    sock: BoxIo,
    enc: ChunkEncoder,
    dec: ChunkDecoder,
    buf: BytesMut,
}

impl Client {
    pub async fn connect(addr: SocketAddr, app: &str) -> Self {
        let mut sock = TcpStream::connect(addr).await.unwrap();
        let ch = handshake::client(&mut sock, 0).await.unwrap();
        assert!(ch.digest.is_some(), "server should answer with a complex handshake");
        Self::after_handshake(Box::new(sock), &format!("rtmp://{addr}/{app}"), app).await
    }

    /// Handshake and `connect` over an already open connection (e.g. TLS).
    pub async fn connect_io(mut sock: BoxIo, tc_url: &str, app: &str) -> Self {
        handshake::client(&mut sock, 0).await.unwrap();
        Self::after_handshake(sock, tc_url, app).await
    }

    async fn after_handshake(sock: BoxIo, tc_url: &str, app: &str) -> Self {
        let mut c = Client {
            sock,
            enc: ChunkEncoder::default(),
            dec: ChunkDecoder::new(1 << 24, 64, true),
            buf: BytesMut::new(),
        };
        c.send(amf_cmd(
            3,
            0,
            &[
                Amf0::str("connect"),
                Amf0::Number(1.0),
                Amf0::object([
                    ("app", Amf0::str(app)),
                    ("tcUrl", Amf0::str(tc_url)),
                    ("objectEncoding", Amf0::Number(0.0)),
                ]),
            ],
        ))
        .await;
        c
    }

    pub async fn send(&mut self, m: Message) {
        let mut out = BytesMut::new();
        self.enc.encode(&m, &mut out);
        if m.type_id == MSG_CHUNK_SIZE {
            self.enc.set_chunk_size(u32::from_be_bytes(m.payload[..4].try_into().unwrap()) as usize);
        }
        self.sock.write_all(&out).await.unwrap();
    }

    /// Next message, transparently applying peer chunk-size changes.
    pub async fn recv(&mut self) -> Option<Message> {
        loop {
            if let Some(m) = self.dec.decode(&mut self.buf).unwrap() {
                if m.type_id == MSG_CHUNK_SIZE {
                    let n = u32::from_be_bytes(m.payload[..4].try_into().unwrap());
                    self.dec.set_chunk_size(n as usize);
                }
                return Some(m);
            }
            let n = timeout(Duration::from_secs(5), self.sock.read_buf(&mut self.buf))
                .await
                .expect("timed out waiting for server")
                .unwrap();
            if n == 0 {
                return None;
            }
        }
    }

    /// Skip messages until an AMF command/data whose first value is `name`.
    pub async fn expect_amf(&mut self, name: &str) -> Vec<Amf0> {
        loop {
            let m = self.recv().await.unwrap_or_else(|| panic!("closed while waiting for {name}"));
            if matches!(m.type_id, MSG_AMF_CMD | MSG_AMF_META) {
                let v = amf::decode_all(&m.payload).unwrap();
                if v.first().and_then(Amf0::as_str) == Some(name) {
                    return v;
                }
            }
        }
    }

    pub async fn expect_status(&mut self) -> String {
        let v = self.expect_amf("onStatus").await;
        v[3].get("code").and_then(Amf0::as_str).unwrap().to_string()
    }

    pub async fn expect_av(&mut self) -> Message {
        loop {
            let m = self.recv().await.expect("closed while waiting for media");
            if matches!(m.type_id, MSG_AUDIO | MSG_VIDEO) {
                return m;
            }
        }
    }

    pub async fn create_stream(&mut self) {
        self.send(amf_cmd(3, 0, &[Amf0::str("createStream"), Amf0::Number(2.0), Amf0::Null])).await;
        let r = self.expect_amf("_result").await;
        assert_eq!(r[3], Amf0::Number(1.0));
    }
}

pub fn av(type_id: u8, ts: u32, payload: &'static [u8]) -> Message {
    Message { csid: if type_id == MSG_VIDEO { 7 } else { 6 }, timestamp: ts, type_id, msid: 1, payload: Bytes::from_static(payload) }
}


impl Client {
    pub async fn publish(&mut self, name: &str) {
        self.send(amf_cmd(8, 1, &[Amf0::str("publish"), Amf0::Number(3.0), Amf0::Null, Amf0::str(name), Amf0::str("live")])).await;
    }

    pub async fn play(&mut self, name: &str) {
        self.send(amf_cmd(8, 1, &[Amf0::str("play"), Amf0::Number(4.0), Amf0::Null, Amf0::str(name)])).await;
    }

    /// connect + createStream, consuming the replies.
    pub async fn ready(addr: SocketAddr, app: &str) -> Self {
        let mut c = Client::connect(addr, app).await;
        c.expect_amf("_result").await;
        c.create_stream().await;
        c
    }
}
