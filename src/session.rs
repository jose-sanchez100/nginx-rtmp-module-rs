//! Per-connection RTMP session.
//!
//! Combines the roles of `ngx_rtmp_init.c` (session setup / teardown),
//! `ngx_rtmp_handler.c` (receive loop, ping, ack), `ngx_rtmp_receive.c`
//! (protocol control messages), `ngx_rtmp_cmd_module.c` (NetConnection /
//! NetStream commands), `ngx_rtmp_access_module.c` (allow/deny) and the
//! session side of `ngx_rtmp_notify_module.c` (HTTP callbacks).
//!
//! Each session runs a reader loop and a separate writer task fed by a
//! bounded channel; the channel bound is the `out_queue` setting.

use crate::amf::{self, Amf0};
use crate::chunk::{ChunkDecoder, ChunkEncoder, Message};
use crate::codec;
use crate::config::{AccessAction, AccessScope, AppConf, MetaMode, RelayTarget, ServerConf};
use crate::handshake;
use crate::live::{LiveError, Peer, Registry, SessionId, StreamRef};
use crate::message::{self as msg, *};
use crate::notify::{self, Form, Reply};
use bytes::{Bytes, BytesMut};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

/// Per-session notify state (`ngx_rtmp_notify_ctx_t`), shared with the
/// `on_update` task.
#[derive(Default)]
struct NotifyCtx {
    publishing: bool,
    playing: bool,
    /// Stream name and arguments as requested by the client (before any
    /// redirect), reported by the `*_done` and `update` callbacks.
    name: String,
    args: String,
}

struct NotifyState {
    ctx: Arc<Mutex<NotifyCtx>>,
    update: Option<tokio::task::JoinHandle<()>>,
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
    swf_url: String,
    page_url: String,
    /// A `connect` command was received (gates `on_disconnect`).
    connect_seen: bool,

    ack_size: u32,
    in_bytes: u32,
    in_last_ack: u32,
    buflen: u32,

    joined: Option<Joined>,
    last_av: Instant,
    /// Timestamp of the last received message (`s->current_time`).
    last_ts: Arc<AtomicU32>,
    notify: Option<NotifyState>,
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
        swf_url: String::new(),
        page_url: String::new(),
        connect_seen: false,
        ack_size: 0,
        in_bytes: 0,
        in_last_ack: 0,
        buflen: conf.buflen.as_millis() as u32,
        joined: None,
        last_av: Instant::now(),
        last_ts: Arc::new(AtomicU32::new(0)),
        notify: None,
    };

    let result = s.read_loop(&mut rd).await;
    s.leave_stream();
    s.notify_done();
    s.notify_disconnect();
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
                        if let Flow::Close = self.handle(m).await? {
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

    async fn handle(&mut self, m: Message) -> Result<Flow> {
        debug!(
            "recv {} ({}) csid={} msid={} ts={} len={}",
            type_name(m.type_id), m.type_id, m.csid, m.msid, m.timestamp, m.payload.len()
        );
        self.last_ts.store(m.timestamp, Ordering::Relaxed);
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
            MSG_AMF_META => return self.handle_data(&m.payload, m.timestamp),
            MSG_AMF3_META => return self.handle_data(m.payload.get(1..).unwrap_or_default(), m.timestamp),
            MSG_AMF_CMD => return self.handle_cmd(&m.payload).await,
            MSG_AMF3_CMD => return self.handle_cmd(m.payload.get(1..).unwrap_or_default()).await,
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
    /// FLV tags only carry audio, video and metadata.
    fn handle_aggregate(&mut self, m: Message) -> Result<Flow> {
        for sub in msg::split_aggregate(&m) {
            self.last_ts.store(sub.timestamp, Ordering::Relaxed);
            let flow = match sub.type_id {
                MSG_AUDIO | MSG_VIDEO => {
                    self.handle_av(sub.type_id, sub.timestamp, sub.payload);
                    Flow::Continue
                }
                MSG_AMF_META => self.handle_data(&sub.payload, sub.timestamp)?,
                MSG_AMF3_META => self.handle_data(sub.payload.get(1..).unwrap_or_default(), sub.timestamp)?,
                t => {
                    debug!("unexpected message type {t} in aggregate");
                    Flow::Continue
                }
            };
            if let Flow::Close = flow {
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

    fn handle_data(&mut self, payload: &[u8], timestamp: u32) -> Result<Flow> {
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
            // nginx sends rebuilt metadata at timestamp 0 and copied metadata
            // at the publisher's timestamp (ngx_rtmp_codec_prepare_meta).
            // Players built on libavformat only treat onMetaData at dts 0 as
            // metadata; anything later becomes an extra data stream that
            // stalls stream probing (VLC starts seconds late and stutters).
            let ts = if app.meta == MetaMode::Copy { timestamp } else { 0 };
            j.stream.lock().unwrap().set_meta(payload, info, ts);
        }
        Ok(Flow::Continue)
    }

    // -----------------------------------------------------------------
    // Commands (ngx_rtmp_cmd_module.c)
    // -----------------------------------------------------------------

    async fn handle_cmd(&mut self, payload: &[u8]) -> Result<Flow> {
        let v = amf::decode_all(payload)
            .map_err(|e| SessionError::Protocol(format!("bad AMF command: {e}")))?;
        let Some(name) = v.first().and_then(Amf0::as_str) else {
            return Err(SessionError::Protocol("AMF command without name".into()));
        };
        let tid = v.get(1).and_then(Amf0::as_f64).unwrap_or(0.0);
        debug!("command {name} tid={tid}");
        let str_arg = |i: usize| v.get(i).and_then(Amf0::as_str).unwrap_or("").to_string();

        match name {
            "connect" => self.cmd_connect(tid, v.get(2)).await,
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
                self.cmd_publish(&str_arg(3), &str_arg(4)).await
            }
            "play" | "play2" => {
                self.require_connected()?;
                let name = if name == "play2" {
                    v.get(3).and_then(|o| o.get("streamName")).and_then(Amf0::as_str).unwrap_or("").to_string()
                } else {
                    str_arg(3)
                };
                self.cmd_play(&name).await
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

    async fn cmd_connect(&mut self, tid: f64, obj: Option<&Amf0>) -> Result<Flow> {
        if self.connected || self.connect_seen {
            return Err(SessionError::Protocol("duplicate connection".into()));
        }
        self.connect_seen = true;
        let get = |k: &str| obj.and_then(|o| o.get(k)).and_then(Amf0::as_str).unwrap_or("").to_string();
        let mut app = get("app");
        if let Some((a, args)) = app.split_once('?') {
            self.args = args.to_string();
            app = a.to_string();
        }
        self.tc_url = get("tcUrl");
        self.flashver = get("flashVer");
        self.swf_url = get("swfUrl");
        self.page_url = get("pageUrl");
        let object_encoding = obj.and_then(|o| o.get("objectEncoding")).and_then(Amf0::as_f64).unwrap_or(0.0);

        info!(
            "connect: app='{app}' args='{}' flashver='{}' tc_url='{}'",
            self.args, self.flashver, self.tc_url
        );

        // on_connect (ngx_rtmp_notify_connect)
        if let Some(url) = self.ctx.conf.notify.on_connect.clone() {
            info!("notify: connect '{}'", url.raw);
            let mut f = Form::new();
            f.add("app", &app)
                .add("flashver", &self.flashver)
                .add("swfurl", &self.swf_url)
                .add("tcurl", &self.tc_url)
                .add("pageurl", &self.page_url)
                .add("addr", self.addr.ip().to_string())
                .add("epoch", epoch_ms().to_string())
                .add("call", "connect")
                .raw(&self.args);
            match notify::call(&url, self.ctx.conf.notify.method, &f, self.ctx.conf.netcall_timeout).await {
                Reply::Ok | Reply::Redirect(None) => {}
                Reply::Redirect(Some(loc)) => {
                    info!("notify: connect redirect to '{loc}'");
                    app = loc;
                }
                Reply::Error(e) => {
                    info!("notify: connect rejected: {e}");
                    return Ok(Flow::Close);
                }
            }
        }

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

    async fn cmd_publish(&mut self, full_name: &str, kind: &str) -> Result<Flow> {
        let (name, args) = full_name.split_once('?').unwrap_or((full_name, ""));
        let mut name = name.to_string();
        info!("publish: name='{name}' args='{args}' type='{kind}'");
        if !self.access_allowed(true) {
            info!("access forbidden: publish");
            return Ok(Flow::Close);
        }
        let app = self.app.clone().expect("connected");

        // on_publish (ngx_rtmp_notify_publish)
        let mut redirect_push = None;
        self.notify_init(&name, args, true);
        if let Some(url) = app.notify.on_publish.clone() {
            info!("notify: publish '{}'", url.raw);
            let mut f = self.notify_form();
            f.add("call", "publish").add("name", &name).add("type", kind).raw(args);
            match self.notify_redirect(&app, &url, &f, "publish", &mut name).await {
                Some(target) => redirect_push = target,
                None => {
                    if let Some(n) = &self.notify {
                        n.ctx.lock().unwrap().publishing = false;
                    }
                    return Ok(Flow::Close);
                }
            }
        }

        if !app.live {
            warn!("publish: 'live' is off in application '{}', stream goes nowhere", app.name);
            return Ok(Flow::Continue);
        }
        self.leave_stream();
        match self.ctx.registry.publish(self.ctx.index, &app, &name, &self.peer) {
            Ok(stream) => {
                let push_stop = crate::relay::start_pushes(&self.ctx, &app, &name, redirect_push);
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

    async fn cmd_play(&mut self, full_name: &str) -> Result<Flow> {
        let (name, args) = full_name.split_once('?').unwrap_or((full_name, ""));
        let mut name = name.to_string();
        info!("play: name='{name}' args='{args}'");
        if !self.access_allowed(false) {
            info!("access forbidden: play");
            return Ok(Flow::Close);
        }
        let app = self.app.clone().expect("connected");

        // on_play (ngx_rtmp_notify_play)
        let mut redirect_pull = None;
        self.notify_init(&name, args, false);
        if let Some(url) = app.notify.on_play.clone() {
            info!("notify: play '{}'", url.raw);
            let mut f = self.notify_form();
            // start/duration/reset are not tracked; live clients leave them
            // at their defaults.
            f.add("call", "play")
                .add("name", &name)
                .add("start", "0")
                .add("duration", "0")
                .add("reset", "0")
                .raw(args);
            match self.notify_redirect(&app, &url, &f, "play", &mut name).await {
                Some(target) => redirect_pull = target,
                None => {
                    if let Some(n) = &self.notify {
                        n.ctx.lock().unwrap().playing = false;
                    }
                    return Ok(Flow::Close);
                }
            }
        }

        if !app.live {
            warn!("play: 'live' is off in application '{}' (VOD not ported yet)", app.name);
            self.send(msg::status("NetStream.Play.StreamNotFound", "error", "No such stream"))?;
            return Ok(Flow::Close);
        }
        self.leave_stream();
        // ngx_rtmp_cmd_stream_begin / ngx_rtmp_live_play
        self.send(msg::stream_begin(MSID))?;
        crate::relay::maybe_pull(&self.ctx, &app, &name, redirect_pull);
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
        self.notify_done();
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

    // -----------------------------------------------------------------
    // Notify (ngx_rtmp_notify_module.c)
    // -----------------------------------------------------------------

    /// Common callback parameters (`ngx_rtmp_netcall_http_format_session`).
    fn notify_form(&self) -> Form {
        let mut f = Form::new();
        f.add("app", &self.app_name)
            .add("flashver", &self.flashver)
            .add("swfurl", &self.swf_url)
            .add("tcurl", &self.tc_url)
            .add("pageurl", &self.page_url)
            .add("addr", self.addr.ip().to_string())
            .add("clientid", self.id.to_string());
        f
    }

    /// `ngx_rtmp_notify_init`: remember the stream for the `*_done` and
    /// `update` callbacks and start the `on_update` timer.
    fn notify_init(&mut self, name: &str, args: &str, publishing: bool) {
        let app = self.app.clone().expect("connected");
        let nc = &app.notify;
        if !nc.active() {
            return;
        }
        let base = self.notify_form();
        let state = self.notify.get_or_insert_with(|| NotifyState { ctx: Default::default(), update: None });
        {
            let mut c = state.ctx.lock().unwrap();
            c.name = name.to_string();
            c.args = args.to_string();
            if publishing {
                c.publishing = true;
            } else {
                c.playing = true;
            }
        }
        let Some(url) = nc.on_update.clone() else { return };
        if nc.update_timeout.is_zero() || state.update.is_some() {
            return;
        }
        let ctx = state.ctx.clone();
        let last_ts = self.last_ts.clone();
        let close = self.peer.close.clone();
        let (method, period, strict, timeout) =
            (nc.method, nc.update_timeout, nc.update_strict, self.ctx.conf.netcall_timeout);
        let task = async move {
            let start = Instant::now();
            loop {
                tokio::time::sleep(period).await;
                let mut f = base.clone();
                {
                    let c = ctx.lock().unwrap();
                    let call = if c.publishing {
                        "update_publish"
                    } else if c.playing {
                        "update_play"
                    } else {
                        "update"
                    };
                    f.add("call", call)
                        .add("time", start.elapsed().as_secs().to_string())
                        .add("timestamp", last_ts.load(Ordering::Relaxed).to_string());
                    if !c.name.is_empty() {
                        f.add("name", &c.name);
                    }
                    f.raw(&c.args);
                }
                info!("notify: update '{}'", url.raw);
                let reply = notify::call(&url, method, &f, timeout).await;
                let failed = match &reply {
                    Reply::Ok => false,
                    Reply::Redirect(_) => strict,
                    Reply::Error(_) => true,
                };
                if failed {
                    info!("notify: update failed: {reply:?}");
                    close.notify_one();
                    return;
                }
            }
        };
        state.update = Some(tokio::spawn(task.in_current_span()));
    }

    /// Send an `on_publish` / `on_play` callback and apply its answer.
    /// Returns `None` to reject, or `Some(target)` to go on, where `target`
    /// is set for a redirect to an `rtmp://` URL (push / pull). A plain 3xx
    /// redirect renames the stream in `name`.
    async fn notify_redirect(
        &self,
        app: &AppConf,
        url: &notify::NotifyUrl,
        form: &Form,
        what: &str,
        name: &mut String,
    ) -> Option<Option<Arc<RelayTarget>>> {
        match notify::call(url, app.notify.method, form, self.ctx.conf.netcall_timeout).await {
            Reply::Ok | Reply::Redirect(None) => Some(None),
            Reply::Redirect(Some(loc)) => {
                if loc.len() < 7 || !loc[..7].eq_ignore_ascii_case("rtmp://") {
                    info!("notify: {what} redirect to '{loc}'");
                    *name = loc;
                    return Some(None);
                }
                if app.notify.relay_redirect {
                    *name = notify::relay_redirect_name(&loc);
                }
                let Some(t) = RelayTarget::parse_url(&loc) else {
                    info!("notify: {what} relay failed '{name}': bad url '{loc}'");
                    return None;
                };
                let relay = if what == "publish" { "push to" } else { "pull from" };
                info!("notify: {what} '{name}' {relay} '{loc}'");
                Some(Some(Arc::new(t)))
            }
            Reply::Error(e) => {
                info!("notify: {what} rejected: {e}");
                None
            }
        }
    }

    /// `ngx_rtmp_notify_close_stream`: fire `on_publish_done` /
    /// `on_play_done` / `on_done` (without waiting) and stop `on_update`.
    fn notify_done(&mut self) {
        let Some(state) = &mut self.notify else { return };
        if let Some(t) = state.update.take() {
            t.abort();
        }
        let (publishing, playing, name, args) = {
            let mut c = state.ctx.lock().unwrap();
            let r = (c.publishing, c.playing, c.name.clone(), c.args.clone());
            c.publishing = false;
            c.playing = false;
            r
        };
        let Some(app) = self.app.clone() else { return };
        let nc = &app.notify;
        let calls = [
            (publishing, "publish_done", &nc.on_publish_done),
            (playing, "play_done", &nc.on_play_done),
            (publishing || playing, "done", &nc.on_done),
        ];
        for (on, call, url) in calls {
            let (true, Some(url)) = (on, url.clone()) else { continue };
            info!("notify: {call} '{}'", url.raw);
            let mut f = self.notify_form();
            f.add("call", call);
            if !name.is_empty() {
                f.add("name", &name);
            }
            f.raw(&args);
            let (method, timeout) = (nc.method, self.ctx.conf.netcall_timeout);
            tokio::spawn(
                async move {
                    if let Reply::Error(e) = notify::call(&url, method, &f, timeout).await {
                        debug!("notify: {call} failed: {e}");
                    }
                }
                .in_current_span(),
            );
        }
    }

    /// `ngx_rtmp_notify_disconnect` (fire and forget).
    fn notify_disconnect(&self) {
        let nc = &self.ctx.conf.notify;
        let (true, Some(url)) = (self.connect_seen, nc.on_disconnect.clone()) else { return };
        info!("notify: disconnect '{}'", url.raw);
        let mut f = self.notify_form();
        f.add("call", "disconnect").raw(&self.args);
        let (method, timeout) = (nc.method, self.ctx.conf.netcall_timeout);
        tokio::spawn(
            async move {
                if let Reply::Error(e) = notify::call(&url, method, &f, timeout).await {
                    debug!("notify: disconnect failed: {e}");
                }
            }
            .in_current_span(),
        );
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
