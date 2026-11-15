//! Per-connection RTMP session.
//!
//! Combines the roles of `ngx_rtmp_init.c` (session setup / teardown),
//! `ngx_rtmp_handler.c` (receive loop, ping, ack), `ngx_rtmp_receive.c`
//! (protocol control messages), `ngx_rtmp_cmd_module.c` (NetConnection /
//! NetStream commands) and `ngx_rtmp_access_module.c` (allow/deny).
//!
//! Each session runs a reader loop and a separate writer task fed by a
//! bounded channel; the channel bound is the `out_queue` setting.

use crate::amf::{self, Amf0};
use crate::chunk::{ChunkDecoder, ChunkEncoder, Message};
use crate::codec;
use crate::config::{AccessAction, AccessScope, AppConf, ServerConf};
use crate::handshake;
use crate::live::{LiveError, Peer, Registry, SessionId, StreamRef};
use crate::message::{self as msg, *};
use bytes::{Bytes, BytesMut};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio::time::{Instant as TokioInstant, sleep_until};
use tracing::{Instrument, debug, info, warn};

pub const FMS_VERSION: &str = "FMS/3,0,1,123";
pub const CAPABILITIES: f64 = 31.0;

/// Shared state for one `server {}` block.
pub struct ServerCtx {
    pub index: usize,
    pub conf: ServerConf,
    pub apps: Vec<Arc<AppConf>>,
    pub registry: Arc<Registry>,
}

impl ServerCtx {
    pub fn new(index: usize, conf: ServerConf, registry: Arc<Registry>) -> Self {
        let apps = conf.applications.iter().cloned().map(Arc::new).collect();
        Self { index, conf, apps, registry }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a session id (also used by relay connections).
pub fn next_session_id() -> SessionId {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn epoch_ms() -> u32 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u32
}

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Chunk(#[from] crate::chunk::ChunkError),
    #[error("output queue overflow")]
    QueueFull,
    #[error("{0}")]
    Protocol(String),
}

type Result<T> = std::result::Result<T, SessionError>;

enum Flow {
    Continue,
    /// Close after flushing queued output.
    Close,
}

struct Joined {
    name: String,
    publishing: bool,
    stream: StreamRef,
    /// Dropping this stops the stream's push relays.
    _push_stop: Option<tokio::sync::watch::Sender<()>>,
}

struct Session {
    id: SessionId,
    addr: SocketAddr,
    ctx: Arc<ServerCtx>,
    peer: Peer,
    decoder: ChunkDecoder,

    connected: bool,
    app: Option<Arc<AppConf>>,
    app_name: String,
    args: String,
    tc_url: String,
    flashver: String,

    ack_size: u32,
    in_bytes: u32,
    in_last_ack: u32,
    buflen: u32,

    joined: Option<Joined>,
    last_av: Instant,
}

/// Run a session on an accepted connection until it closes.
pub async fn serve<S>(io: S, addr: SocketAddr, ctx: Arc<ServerCtx>)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let id = next_session_id();
    let span = tracing::info_span!("session", id, client = %addr);
    async move {
        info!("connected");
        match run(io, id, addr, ctx).await {
            Ok(()) => info!("disconnected"),
            Err(e) => info!("disconnected: {e}"),
        }
    }
    .instrument(span)
    .await
}

async fn run<S>(mut io: S, id: SessionId, addr: SocketAddr, ctx: Arc<ServerCtx>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let conf = &ctx.conf;
    tokio::time::timeout(conf.timeout, handshake::server(&mut io, epoch_ms()))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "handshake timed out"))??;
    debug!("handshake done");

    let (mut rd, wr) = tokio::io::split(io);
    let (tx, rx) = mpsc::channel(conf.out_queue.max(8));
    let close = Arc::new(Notify::new());
    let writer = tokio::spawn(writer_task(wr, rx, conf.timeout, close.clone()).in_current_span());

    let mut s = Session {
        id,
        addr,
        ctx: ctx.clone(),
        peer: Peer { id, tx, close: close.clone() },
        decoder: ChunkDecoder::new(conf.max_message, conf.max_streams, conf.publish_time_fix),
        connected: false,
        app: None,
        app_name: String::new(),
        args: String::new(),
        tc_url: String::new(),
        flashver: String::new(),
        ack_size: 0,
        in_bytes: 0,
        in_last_ack: 0,
        buflen: conf.buflen.as_millis() as u32,
        joined: None,
        last_av: Instant::now(),
    };

    let result = s.read_loop(&mut rd).await;
    s.leave_stream();
    let Session { peer, .. } = s;
    drop(peer); // closes the channel; writer flushes what is queued and exits
    let _ = tokio::time::timeout(conf.timeout, writer).await;
    result
}

async fn writer_task<W: AsyncWrite + Unpin>(
    mut w: W,
    mut rx: mpsc::Receiver<Message>,
    timeout: Duration,
    close: Arc<Notify>,
) {
    let mut enc = ChunkEncoder::default();
    let mut buf = BytesMut::with_capacity(64 * 1024);
    let push = |enc: &mut ChunkEncoder, buf: &mut BytesMut, m: Message| {
        enc.encode(&m, buf);
        if m.type_id == MSG_CHUNK_SIZE && m.csid == CSID_CTL && m.payload.len() >= 4 {
            let size = u32::from_be_bytes(m.payload[..4].try_into().unwrap());
            enc.set_chunk_size(size as usize);
        }
    };
    while let Some(m) = rx.recv().await {
        push(&mut enc, &mut buf, m);
        // Coalesce whatever else is already queued into one write.
        while buf.len() < 256 * 1024 {
            match rx.try_recv() {
                Ok(m) => push(&mut enc, &mut buf, m),
                Err(_) => break,
            }
        }
        match tokio::time::timeout(timeout, w.write_all(&buf)).await {
            Ok(Ok(())) => buf.clear(),
            Ok(Err(e)) => {
                debug!("write error: {e}");
                close.notify_one();
                return;
            }
            Err(_) => {
                info!("client timed out (send)");
                close.notify_one();
                return;
            }
        }
    }
    let _ = w.flush().await;
    let _ = w.shutdown().await;
}

impl Session {
    fn conf(&self) -> &ServerConf {
        &self.ctx.conf
    }

    fn send(&self, m: Message) -> Result<()> {
        self.peer.tx.try_send(m).map_err(|_| SessionError::QueueFull)
    }

    async fn read_loop<R: AsyncRead + Unpin>(&mut self, rd: &mut R) -> Result<()> {
        let ping = self.conf().ping;
        let ping_timeout = self.conf().ping_timeout;
        let mut buf = BytesMut::with_capacity(64 * 1024);
        let mut got_data = false;
        let mut ping_active = false;
        let far = Duration::from_secs(86400 * 365);
        let mut ping_deadline = TokioInstant::now() + if ping.is_zero() { far } else { ping };
        let mut idle_check = tokio::time::interval(Duration::from_secs(1));
        let close = self.peer.close.clone();

        loop {
            tokio::select! {
                _ = close.notified() => {
                    // Writer failed (peer gone / send timeout) or a stream
                    // dropped us (mandatory frame lost).
                    return Err(SessionError::Protocol("output closed".into()));
                }
                _ = sleep_until(ping_deadline) => {
                    // ngx_rtmp_ping()
                    if ping_active {
                        if !got_data {
                            return Err(SessionError::Protocol("ping timeout".into()));
                        }
                        ping_active = false;
                        got_data = false;
                        ping_deadline = TokioInstant::now() + ping;
                    } else if got_data && !self.conf().busy {
                        got_data = false;
                        ping_deadline = TokioInstant::now() + ping;
                    } else {
                        debug!("sending ping");
                        ping_active = true;
                        got_data = false;
                        self.send(msg::ping_request(epoch_ms()))?;
                        ping_deadline = TokioInstant::now() + ping_timeout;
                    }
                }
                _ = idle_check.tick() => {
                    if let (Some(j), Some(app)) = (&self.joined, &self.app)
                        && let Some(limit) = app.drop_idle_publisher
                            && j.publishing && self.last_av.elapsed() > limit {
                                info!("drop idle publisher");
                                return Ok(());
                            }
                }
                r = rd.read_buf(&mut buf) => {
                    let n = r?;
                    if n == 0 {
                        return Ok(());
                    }
                    got_data = true;
                    self.account_in(n)?;
                    while let Some(m) = self.decoder.decode(&mut buf)? {
                        if let Flow::Close = self.handle(m)? {
                            return Ok(());
                        }
                    }
                    if buf.capacity() - buf.len() < 4096 {
                        buf.reserve(64 * 1024);
                    }
                }
            }
        }
    }

    fn account_in(&mut self, n: usize) -> Result<()> {
        self.in_bytes = self.in_bytes.wrapping_add(n as u32);
        if self.in_bytes >= 0xf000_0000 {
            debug!("resetting byte counter");
            self.in_bytes = 0;
            self.in_last_ack = 0;
        }
        if self.ack_size > 0 && self.in_bytes - self.in_last_ack >= self.ack_size {
            self.in_last_ack = self.in_bytes;
            self.send(msg::ack(self.in_bytes))?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Message dispatch (ngx_rtmp_receive_message)
    // -----------------------------------------------------------------

    fn handle(&mut self, m: Message) -> Result<Flow> {
        debug!(
            "recv {} ({}) csid={} msid={} ts={} len={}",
            type_name(m.type_id), m.type_id, m.csid, m.msid, m.timestamp, m.payload.len()
        );
        match m.type_id {
            MSG_CHUNK_SIZE => {
                let v = be_u32(&m.payload)? & 0x7fff_ffff;
                debug!("peer chunk size {v}");
                self.decoder.set_chunk_size(v as usize);
            }
            MSG_ABORT => self.decoder.abort(be_u32(&m.payload)?),
            MSG_ACK => {}
            MSG_ACK_SIZE => self.ack_size = be_u32(&m.payload)?,
            MSG_BANDWIDTH => {}
            MSG_USER => self.handle_user(&m.payload)?,
            MSG_AUDIO | MSG_VIDEO => self.handle_av(m.type_id, m.timestamp, m.payload),
            MSG_AMF_META => return self.handle_data(&m.payload),
            MSG_AMF3_META => return self.handle_data(m.payload.get(1..).unwrap_or_default()),
            MSG_AMF_CMD => return self.handle_cmd(&m.payload),
            MSG_AMF3_CMD => return self.handle_cmd(m.payload.get(1..).unwrap_or_default()),
            MSG_AGGREGATE => return self.handle_aggregate(m),
            MSG_AMF_SHARED | MSG_AMF3_SHARED | MSG_EDGE => {}
            t => debug!("unhandled message type {t}"),
        }
        Ok(Flow::Continue)
    }

    fn handle_user(&mut self, p: &[u8]) -> Result<()> {
        if p.len() < 2 {
            return Err(SessionError::Protocol("short user control message".into()));
        }
        let evt = u16::from_be_bytes([p[0], p[1]]);
        let arg = |i: usize| -> u32 {
            p.get(2 + 4 * i..6 + 4 * i).map(|b| u32::from_be_bytes(b.try_into().unwrap())).unwrap_or(0)
        };
        match evt {
            USER_SET_BUFLEN => {
                self.buflen = arg(1);
                debug!("set buflen msid={} buflen={}", arg(0), self.buflen);
            }
            USER_PING_REQUEST => self.send(msg::ping_response(arg(0)))?,
            USER_PING_RESPONSE => debug!("ping response {}", arg(0)),
            _ => debug!("user event {evt}"),
        }
        Ok(())
    }

    /// Aggregate message: a sequence of FLV tags (`ngx_rtmp_aggregate_message_handler`).
    fn handle_aggregate(&mut self, m: Message) -> Result<Flow> {
        for sub in msg::split_aggregate(&m) {
            if let Flow::Close = self.handle(sub)? {
                return Ok(Flow::Close);
            }
        }
        Ok(Flow::Continue)
    }

    fn handle_av(&mut self, type_id: u8, ts: u32, payload: Bytes) {
        let Some(j) = &self.joined else { return };
        if !j.publishing {
            return;
        }
        self.last_av = Instant::now();
        j.stream.lock().unwrap().on_av(type_id, ts, payload);
    }

    fn handle_data(&mut self, payload: &[u8]) -> Result<Flow> {
        let values = match amf::decode_all(payload) {
            Ok(v) => v,
            Err(e) => {
                warn!("bad AMF data message: {e}");
                return Ok(Flow::Continue);
            }
        };
        let (Some(j), Some(app)) = (&self.joined, &self.app) else {
            return Ok(Flow::Continue);
        };
        if !j.publishing {
            return Ok(Flow::Continue);
        }
        if let Some((payload, info)) = codec::process_metadata(&values, app.meta) {
            debug!("metadata: {info:?}");
            j.stream.lock().unwrap().set_meta(payload, info);
        }
        Ok(Flow::Continue)
    }

    // -----------------------------------------------------------------
    // Commands (ngx_rtmp_cmd_module.c)
    // -----------------------------------------------------------------

    fn handle_cmd(&mut self, payload: &[u8]) -> Result<Flow> {
        let v = amf::decode_all(payload)
            .map_err(|e| SessionError::Protocol(format!("bad AMF command: {e}")))?;
        let Some(name) = v.first().and_then(Amf0::as_str) else {
            return Err(SessionError::Protocol("AMF command without name".into()));
        };
        let tid = v.get(1).and_then(Amf0::as_f64).unwrap_or(0.0);
        debug!("command {name} tid={tid}");
        let str_arg = |i: usize| v.get(i).and_then(Amf0::as_str).unwrap_or("").to_string();

        match name {
            "connect" => self.cmd_connect(tid, v.get(2)),
            "createStream" => {
                self.require_connected()?;
                self.send(amf_cmd(
                    CSID_AMF_INI,
                    0,
                    &[Amf0::str("_result"), Amf0::Number(tid), Amf0::Null, Amf0::Number(MSID as f64)],
                ))?;
                Ok(Flow::Continue)
            }
            "publish" => {
                self.require_connected()?;
                self.cmd_publish(&str_arg(3), &str_arg(4))
            }
            "play" | "play2" => {
                self.require_connected()?;
                let name = if name == "play2" {
                    v.get(3).and_then(|o| o.get("streamName")).and_then(Amf0::as_str).unwrap_or("").to_string()
                } else {
                    str_arg(3)
                };
                self.cmd_play(&name)
            }
            "closeStream" | "deleteStream" => {
                self.cmd_close_stream()?;
                Ok(Flow::Continue)
            }
            "pause" | "pauseraw" => {
                let pause = v.get(3).and_then(Amf0::as_bool).unwrap_or(false);
                self.cmd_pause(pause)?;
                Ok(Flow::Continue)
            }
            // Accepted and ignored by nginx-rtmp as well.
            "releaseStream" | "FCPublish" | "FCUnpublish" | "FCSubscribe" | "getStreamLength"
            | "_checkbw" | "seek" | "receiveAudio" | "receiveVideo" => Ok(Flow::Continue),
            other => {
                debug!("unhandled command \"{other}\"");
                Ok(Flow::Continue)
            }
        }
    }

    fn require_connected(&self) -> Result<()> {
        if self.connected { Ok(()) } else { Err(SessionError::Protocol("command before connect".into())) }
    }

    fn cmd_connect(&mut self, tid: f64, obj: Option<&Amf0>) -> Result<Flow> {
        if self.connected {
            return Err(SessionError::Protocol("duplicate connection".into()));
        }
        let get = |k: &str| obj.and_then(|o| o.get(k)).and_then(Amf0::as_str).unwrap_or("").to_string();
        let mut app = get("app");
        if let Some((a, args)) = app.split_once('?') {
            self.args = args.to_string();
            app = a.to_string();
        }
        self.tc_url = get("tcUrl");
        self.flashver = get("flashVer");
        let object_encoding = obj.and_then(|o| o.get("objectEncoding")).and_then(Amf0::as_f64).unwrap_or(0.0);

        info!(
            "connect: app='{app}' args='{}' flashver='{}' tc_url='{}'",
            self.args, self.flashver, self.tc_url
        );

        let Some(conf) = self.ctx.apps.iter().find(|a| a.name == app).cloned() else {
            info!("connect: application not found: '{app}'");
            return Ok(Flow::Close);
        };
        self.app = Some(conf);
        self.app_name = app;
        self.connected = true;

        let c = self.conf();
        let (ack_window, chunk_size) = (c.ack_window, c.chunk_size as u32);
        self.send(msg::ack_size(ack_window))?;
        self.send(msg::bandwidth(ack_window, LIMIT_DYNAMIC))?;
        self.send(msg::chunk_size(chunk_size))?;
        self.send(amf_cmd(
            CSID_AMF_INI,
            0,
            &[
                Amf0::str("_result"),
                Amf0::Number(tid),
                Amf0::object([
                    ("fmsVer", Amf0::str(FMS_VERSION)),
                    ("capabilities", Amf0::Number(CAPABILITIES)),
                ]),
                Amf0::object([
                    ("level", Amf0::str("status")),
                    ("code", Amf0::str("NetConnection.Connect.Success")),
                    ("description", Amf0::str("Connection succeeded.")),
                    ("objectEncoding", Amf0::Number(object_encoding)),
                ]),
            ],
        ))?;
        Ok(Flow::Continue)
    }

    /// `ngx_rtmp_access_module`: first matching rule wins, default allow.
    fn access_allowed(&self, publishing: bool) -> bool {
        let Some(app) = &self.app else { return false };
        let ip = self.addr.ip();
        for r in &app.access {
            let scope_ok = match r.scope {
                AccessScope::Both => true,
                AccessScope::Publish => publishing,
                AccessScope::Play => !publishing,
            };
            if scope_ok && r.matches(ip) {
                return r.action == AccessAction::Allow;
            }
        }
        true
    }

    fn cmd_publish(&mut self, name: &str, kind: &str) -> Result<Flow> {
        let name = name.split_once('?').map_or(name, |(n, _)| n).to_string();
        info!("publish: name='{name}' type='{kind}'");
        if !self.access_allowed(true) {
            info!("access forbidden: publish");
            return Ok(Flow::Close);
        }
        let app = self.app.clone().expect("connected");
        if !app.live {
            warn!("publish: 'live' is off in application '{}', stream goes nowhere", app.name);
            return Ok(Flow::Continue);
        }
        self.leave_stream();
        match self.ctx.registry.publish(self.ctx.index, &app, &name, &self.peer) {
            Ok(stream) => {
                let push_stop = crate::relay::start_pushes(&self.ctx, &app, &name);
                self.joined = Some(Joined { name, publishing: true, stream, _push_stop: push_stop });
                self.last_av = Instant::now();
                self.send(msg::status("NetStream.Publish.Start", "status", "Start publishing"))?;
                Ok(Flow::Continue)
            }
            Err(LiveError::AlreadyPublishing) => {
                info!("live: already publishing");
                self.send(msg::status("NetStream.Publish.BadName", "error", "Already publishing"))?;
                Ok(Flow::Close)
            }
            Err(e) => Err(SessionError::Protocol(e.to_string())),
        }
    }

    fn cmd_play(&mut self, name: &str) -> Result<Flow> {
        let name = name.split_once('?').map_or(name, |(n, _)| n).to_string();
        info!("play: name='{name}'");
        if !self.access_allowed(false) {
            info!("access forbidden: play");
            return Ok(Flow::Close);
        }
        let app = self.app.clone().expect("connected");
        if !app.live {
            warn!("play: 'live' is off in application '{}' (VOD not ported yet)", app.name);
            self.send(msg::status("NetStream.Play.StreamNotFound", "error", "No such stream"))?;
            return Ok(Flow::Close);
        }
        self.leave_stream();
        // ngx_rtmp_cmd_stream_begin / ngx_rtmp_live_play
        self.send(msg::stream_begin(MSID))?;
        crate::relay::maybe_pull(&self.ctx, &app, &name);
        match self.ctx.registry.play(self.ctx.index, &app, &name, &self.peer) {
            Ok(stream) => {
                if !app.play_restart {
                    self.send(msg::status("NetStream.Play.Start", "status", "Start live"))?;
                    self.send(msg::sample_access())?;
                }
                self.joined = Some(Joined { name, publishing: false, stream, _push_stop: None });
                Ok(Flow::Continue)
            }
            Err(LiveError::NotFound) => {
                self.send(msg::status("NetStream.Play.StreamNotFound", "error", "No such stream"))?;
                Ok(Flow::Close)
            }
            Err(e) => Err(SessionError::Protocol(e.to_string())),
        }
    }

    fn cmd_close_stream(&mut self) -> Result<()> {
        let Some(j) = &self.joined else { return Ok(()) };
        let publishing = j.publishing;
        let play_restart = self.app.as_ref().is_some_and(|a| a.play_restart);
        self.leave_stream();
        if publishing {
            self.send(msg::status("NetStream.Unpublish.Success", "status", "Stop publishing"))?;
        } else if !play_restart {
            self.send(msg::status("NetStream.Play.Stop", "status", "Stop live"))?;
        }
        Ok(())
    }

    fn cmd_pause(&mut self, pause: bool) -> Result<()> {
        let Some(j) = &self.joined else { return Ok(()) };
        if j.publishing {
            return Ok(());
        }
        j.stream.lock().unwrap().set_paused(self.id, pause);
        if pause {
            self.send(msg::status("NetStream.Pause.Notify", "status", "Paused live"))?;
        } else {
            self.send(msg::status("NetStream.Unpause.Notify", "status", "Unpaused live"))?;
        }
        Ok(())
    }

    fn leave_stream(&mut self) {
        if let Some(j) = self.joined.take() {
            debug!("leaving stream '{}' (publishing={})", j.name, j.publishing);
            drop(j.stream);
            self.ctx.registry.leave(self.ctx.index, &self.app_name, &j.name, self.id);
        }
    }
}

fn be_u32(p: &[u8]) -> Result<u32> {
    p.get(..4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| SessionError::Protocol("short control message".into()))
}
