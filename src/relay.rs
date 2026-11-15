//! `push` / `pull` relaying.
//!
//! Port of `ngx_rtmp_relay_module.c`:
//!
//! * **push**: when a client publishes a stream locally, an outgoing
//!   connection is opened to every matching `push` target; it publishes
//!   there and is fed as a local player of the stream. Failed or dropped
//!   pushes are retried every `push_reconnect` while the local publisher is
//!   still there.
//! * **pull**: when a client plays a stream that nobody publishes locally,
//!   the first matching `pull` target is played from the upstream server and
//!   republished locally. It is closed when the last local player leaves;
//!   if the upstream drops, local players are disconnected (nginx's
//!   "play disconnect orphan").
//! * **static pull**: started with the server, kept alive forever,
//!   reconnecting every `pull_reconnect`.

use crate::amf::{self, Amf0};
use crate::chunk::{ChunkDecoder, ChunkEncoder, Message};
use crate::codec;
use crate::config::{AppConf, MetaMode, RelayTarget, ServerConf};
use crate::handshake;
use crate::control::Control;
use crate::live::{Peer, PeerInfo, StreamRef};
use crate::message::{self as msg, *};
use crate::session::{ServerCtx, next_session_id};
use bytes::BytesMut;
use std::sync::Arc;
use std::time::Duration;
use crate::tls::{self, BoxIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{Notify, mpsc, watch};
use tracing::{Instrument, debug, info, warn};

pub const RELAY_FLASHVER: &str = "LNX.11,1,102,55";
const CONNECT_TRANS: f64 = 1.0;
const CREATE_STREAM_TRANS: f64 = 2.0;

type Result<T> = std::result::Result<T, RelayError>;

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Chunk(#[from] crate::chunk::ChunkError),
    #[error("timed out")]
    Timeout,
    #[error("upstream closed the connection")]
    Closed,
    #[error("upstream refused: {0}")]
    Refused(String),
    #[error("{0}")]
    Local(String),
}

// ---------------------------------------------------------------------------
// Outgoing RTMP connection
// ---------------------------------------------------------------------------

/// Read side of a relay connection. Protocol-control messages are handled
/// internally; replies they require (acks, pongs) are queued in `replies`.
struct Reader {
    rd: ReadHalf<BoxIo>,
    dec: ChunkDecoder,
    buf: BytesMut,
    ack_size: u32,
    in_bytes: u32,
    in_last_ack: u32,
    replies: Vec<Message>,
}

struct Writer {
    wr: WriteHalf<BoxIo>,
    enc: ChunkEncoder,
    out: BytesMut,
    timeout: Duration,
}

impl Reader {
    /// Next non-protocol-control message. Cancel-safe.
    async fn recv(&mut self) -> Result<Message> {
        loop {
            while let Some(m) = self.dec.decode(&mut self.buf)? {
                match m.type_id {
                    MSG_CHUNK_SIZE if m.payload.len() >= 4 => {
                        let n = u32::from_be_bytes(m.payload[..4].try_into().unwrap()) & 0x7fff_ffff;
                        self.dec.set_chunk_size(n as usize);
                    }
                    MSG_ABORT if m.payload.len() >= 4 => {
                        self.dec.abort(u32::from_be_bytes(m.payload[..4].try_into().unwrap()));
                    }
                    MSG_ACK_SIZE if m.payload.len() >= 4 => {
                        self.ack_size = u32::from_be_bytes(m.payload[..4].try_into().unwrap());
                    }
                    MSG_ACK | MSG_BANDWIDTH => {}
                    MSG_USER if m.payload.len() >= 6 => {
                        let evt = u16::from_be_bytes([m.payload[0], m.payload[1]]);
                        if evt == USER_PING_REQUEST {
                            let ts = u32::from_be_bytes(m.payload[2..6].try_into().unwrap());
                            self.replies.push(msg::ping_response(ts));
                        }
                        return Ok(m);
                    }
                    _ => return Ok(m),
                }
            }
            let n = self.rd.read_buf(&mut self.buf).await?;
            if n == 0 {
                return Err(RelayError::Closed);
            }
            self.in_bytes = self.in_bytes.wrapping_add(n as u32);
            if self.in_bytes >= 0xf000_0000 {
                self.in_bytes = 0;
                self.in_last_ack = 0;
            }
            if self.ack_size > 0 && self.in_bytes - self.in_last_ack >= self.ack_size {
                self.in_last_ack = self.in_bytes;
                self.replies.push(msg::ack(self.in_bytes));
            }
            if self.buf.capacity() - self.buf.len() < 4096 {
                self.buf.reserve(64 * 1024);
            }
        }
    }
}

impl Writer {
    fn queue(&mut self, m: &Message) {
        self.enc.encode(m, &mut self.out);
        if m.type_id == MSG_CHUNK_SIZE && m.csid == CSID_CTL && m.payload.len() >= 4 {
            let n = u32::from_be_bytes(m.payload[..4].try_into().unwrap());
            self.enc.set_chunk_size(n as usize);
        }
    }

    async fn flush(&mut self) -> Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        tokio::time::timeout(self.timeout, self.wr.write_all(&self.out))
            .await
            .map_err(|_| RelayError::Timeout)??;
        self.out.clear();
        Ok(())
    }

    async fn send(&mut self, m: &Message) -> Result<()> {
        self.queue(m);
        self.flush().await
    }

    async fn send_replies(&mut self, r: &mut Reader) -> Result<()> {
        if r.replies.is_empty() {
            return Ok(());
        }
        for m in std::mem::take(&mut r.replies) {
            self.queue(&m);
        }
        self.flush().await
    }
}

/// Status object of an `onStatus` / `_error` command, if `m` is one.
fn status_info(m: &Message) -> Option<(String, String, String)> {
    if m.type_id != MSG_AMF_CMD && m.type_id != MSG_AMF_META {
        return None;
    }
    let v = amf::decode_all(&m.payload).ok()?;
    let name = v.first()?.as_str()?;
    if name != "onStatus" && name != "_error" {
        return None;
    }
    let info = v.iter().rev().find(|x| x.get("code").is_some())?;
    let s = |k| info.get(k).and_then(Amf0::as_str).unwrap_or("").to_string();
    Some((s("level"), s("code"), s("description")))
}

/// Connected relay session: handshake, `connect`, `createStream` done.
struct Conn {
    rd: Reader,
    wr: Writer,
    /// Message stream id returned by the upstream's `createStream`.
    msid: u32,
}

impl Conn {
    /// `ngx_rtmp_relay_create_connection` + `send_connect` + `send_create_stream`.
    async fn open(t: &RelayTarget, local_app: &str, conf: &ServerConf) -> Result<Self> {
        tokio::time::timeout(conf.timeout, Self::open_inner(t, local_app, conf))
            .await
            .map_err(|_| RelayError::Timeout)?
    }

    async fn open_inner(t: &RelayTarget, local_app: &str, conf: &ServerConf) -> Result<Self> {
        let mut sock = tls::connect(&t.host, t.port, t.tls).await?;
        handshake::client(&mut sock, 0).await?;
        let (rd, wr) = tokio::io::split(sock);
        let mut c = Conn {
            rd: Reader {
                rd,
                dec: ChunkDecoder::new(conf.max_message, conf.max_streams, conf.publish_time_fix),
                buf: BytesMut::with_capacity(64 * 1024),
                ack_size: 0,
                in_bytes: 0,
                in_last_ack: 0,
                replies: Vec::new(),
            },
            wr: Writer { wr, enc: ChunkEncoder::default(), out: BytesMut::new(), timeout: conf.timeout },
            msid: MSID,
        };

        let app = if t.app.is_empty() { local_app } else { &t.app };
        let tc_url = if t.tc_url.is_empty() {
            let hostport = t.url.split('/').next().unwrap_or(&t.url);
            let scheme = if t.tls { "rtmps" } else { "rtmp" };
            format!("{scheme}://{hostport}/{app}")
        } else {
            t.tc_url.clone()
        };
        let flash_ver = if t.flash_ver.is_empty() { RELAY_FLASHVER } else { &t.flash_ver };

        c.wr.queue(&msg::chunk_size(conf.chunk_size as u32));
        c.wr.queue(&msg::ack_size(conf.ack_window));
        c.wr.queue(&amf_cmd(
            CSID_AMF_INI,
            0,
            &[
                Amf0::str("connect"),
                Amf0::Number(CONNECT_TRANS),
                Amf0::object([
                    ("app", Amf0::str(app)),
                    ("tcUrl", Amf0::str(tc_url)),
                    ("pageUrl", Amf0::str(t.page_url.clone())),
                    ("swfUrl", Amf0::str(t.swf_url.clone())),
                    ("flashVer", Amf0::str(flash_ver)),
                    ("audioCodecs", Amf0::Number(3575.0)),
                    ("videoCodecs", Amf0::Number(252.0)),
                ]),
            ],
        ));
        c.wr.flush().await?;
        c.wait_result(CONNECT_TRANS).await?;

        c.wr
            .send(&amf_cmd(
                CSID_AMF_INI,
                0,
                &[Amf0::str("createStream"), Amf0::Number(CREATE_STREAM_TRANS), Amf0::Null],
            ))
            .await?;
        let r = c.wait_result(CREATE_STREAM_TRANS).await?;
        if let Some(id) = r.get(3).and_then(Amf0::as_f64) {
            c.msid = id as u32;
        }
        Ok(c)
    }

    /// Wait for `_result` (or `_error`) with transaction id `tid`.
    async fn wait_result(&mut self, tid: f64) -> Result<Vec<Amf0>> {
        loop {
            let m = self.rd.recv().await?;
            self.wr.send_replies(&mut self.rd).await?;
            if m.type_id != MSG_AMF_CMD {
                continue;
            }
            let Ok(v) = amf::decode_all(&m.payload) else { continue };
            let name = v.first().and_then(Amf0::as_str).unwrap_or("");
            if v.get(1).and_then(Amf0::as_f64) != Some(tid) {
                continue;
            }
            match name {
                "_result" => return Ok(v),
                "_error" => {
                    let code = status_info(&m).map(|s| s.1).unwrap_or_default();
                    return Err(RelayError::Refused(format!("_error {code}")));
                }
                _ => {}
            }
        }
    }

    fn stream_name<'a>(t: &'a RelayTarget, local: &'a str) -> &'a str {
        if t.play_path.is_empty() { local } else { &t.play_path }
    }
}

// ---------------------------------------------------------------------------
// push
// ---------------------------------------------------------------------------

/// Start pushes for a freshly published local stream: the configured ones
/// plus `extra` (an `on_publish` redirect to an `rtmp://` URL). Dropping the
/// returned sender (when the publisher leaves) stops them.
pub fn start_pushes(
    ctx: &Arc<ServerCtx>,
    app: &Arc<AppConf>,
    name: &str,
    extra: Option<Arc<RelayTarget>>,
) -> Option<watch::Sender<()>> {
    let targets: Vec<_> = extra.into_iter().chain(app.pushes.iter().filter(|t| t.matches(name)).cloned()).collect();
    if targets.is_empty() {
        return None;
    }
    let (tx, rx) = watch::channel(());
    for t in targets {
        let span = tracing::info_span!("push", stream = %name, url = %t.url);
        tokio::spawn(
            push_loop(ctx.clone(), app.clone(), name.to_string(), t, rx.clone()).instrument(span),
        );
    }
    Some(tx)
}

async fn push_loop(
    ctx: Arc<ServerCtx>,
    app: Arc<AppConf>,
    name: String,
    t: Arc<RelayTarget>,
    mut stop: watch::Receiver<()>,
) {
    loop {
        tokio::select! {
            r = push_once(&ctx, &app, &name, &t) => match r {
                Ok(()) => info!("push ended"),
                Err(e) => warn!("push failed: {e}"),
            },
            _ = stop.changed() => {
                debug!("publisher gone, push stopped");
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(app.push_reconnect) => info!("push reconnect"),
            _ = stop.changed() => return,
        }
    }
}

async fn push_once(ctx: &Arc<ServerCtx>, app: &Arc<AppConf>, name: &str, t: &RelayTarget) -> Result<()> {
    let conf = &ctx.conf;
    let mut c = Conn::open(t, &app.name, conf).await?;
    let remote_name = Conn::stream_name(t, name);
    info!("push connected, publishing as '{}' in app '{}'", remote_name, if t.app.is_empty() { &app.name } else { &t.app });
    c.wr
        .send(&amf_cmd(
            CSID_AMF,
            c.msid,
            &[Amf0::str("publish"), Amf0::Number(0.0), Amf0::Null, Amf0::str(remote_name), Amf0::str("live")],
        ))
        .await?;

    // Join the local stream as a player (`ngx_rtmp_relay_play_local`).
    let id = next_session_id();
    let (tx, mut rx) = mpsc::channel(conf.out_queue.max(8));
    let close = Arc::new(Notify::new());
    let peer = Peer { id, tx, close: close.clone(), info: Arc::new(PeerInfo::new(t.url.clone(), None)) };
    ctx.registry
        .play(ctx.index, app, name, &peer)
        .map_err(|e| RelayError::Local(e.to_string()))?;
    drop(peer);

    let r = async {
        loop {
            tokio::select! {
                m = rx.recv() => {
                    let Some(mut m) = m else { return Ok(()) };
                    // Only media and metadata go upstream; player-facing
                    // control/status messages from the live module do not.
                    let forward = |m: &mut Message, w: &mut Writer| {
                        if matches!(m.type_id, MSG_AUDIO | MSG_VIDEO | MSG_AMF_META) {
                            m.msid = c.msid;
                            w.queue(m);
                        }
                    };
                    forward(&mut m, &mut c.wr);
                    while c.wr.out.len() < 256 * 1024 {
                        match rx.try_recv() {
                            Ok(mut m) => forward(&mut m, &mut c.wr),
                            Err(_) => break,
                        }
                    }
                    c.wr.flush().await?;
                }
                m = c.rd.recv() => {
                    let m = m?;
                    c.wr.send_replies(&mut c.rd).await?;
                    if let Some((level, code, desc)) = status_info(&m) {
                        info!("upstream status: {level} {code} {desc}");
                        if level == "error" {
                            return Err(RelayError::Refused(code));
                        }
                    }
                }
                _ = close.notified() => {
                    return Err(RelayError::Local("dropped (queue overflow or control request)".into()));
                }
            }
        }
    }
    .await;
    ctx.registry.leave(ctx.index, &app.name, name, id);
    r
}

// ---------------------------------------------------------------------------
// pull
// ---------------------------------------------------------------------------

/// Called when a local client plays `name`: start a pull if configured (or
/// requested by an `on_play` redirect, `target`) and the stream has no local
/// publisher or running pull.
pub fn maybe_pull(ctx: &Arc<ServerCtx>, app: &Arc<AppConf>, name: &str, target: Option<Arc<RelayTarget>>) {
    // nginx uses only the first matching target (later ones find the relay
    // context already registered under this name).
    let Some(t) = target.or_else(|| app.pulls.iter().find(|t| t.matches(name)).cloned()) else { return };
    if ctx
        .registry
        .get(ctx.index, &app.name, name)
        .is_some_and(|s| s.lock().unwrap().has_publisher())
    {
        return;
    }
    if !ctx.registry.begin_pull(ctx.index, &app.name, name) {
        return;
    }
    let span = tracing::info_span!("pull", stream = %name, url = %t.url);
    let (ctx, app, name) = (ctx.clone(), app.clone(), name.to_string());
    tokio::spawn(
        async move {
            match pull_once(&ctx, &app, &name, &t, false).await {
                Ok(()) => info!("pull ended"),
                Err(e) => warn!("pull failed: {e}"),
            }
            // Orphaned players are disconnected, as in nginx.
            if let Some(s) = ctx.registry.get(ctx.index, &app.name, &name) {
                let s = s.lock().unwrap();
                if !s.has_publisher() {
                    s.kill_players();
                }
            }
            ctx.registry.end_pull(ctx.index, &app.name, &name);
        }
        .instrument(span),
    );
}

/// Start every `pull ... static` of a server block.
pub fn start_static_pulls(ctx: &Arc<ServerCtx>) {
    for app in &ctx.apps {
        for t in &app.static_pulls {
            let span = tracing::info_span!("static_pull", stream = %t.name, url = %t.url);
            let (ctx, app, t) = (ctx.clone(), app.clone(), t.clone());
            tokio::spawn(
                async move {
                    loop {
                        match pull_once(&ctx, &app, &t.name, &t, true).await {
                            Ok(()) => info!("static pull ended"),
                            Err(e) => warn!("static pull failed: {e}"),
                        }
                        tokio::time::sleep(app.pull_reconnect).await;
                    }
                }
                .instrument(span),
            );
        }
    }
}

async fn pull_once(
    ctx: &Arc<ServerCtx>,
    app: &Arc<AppConf>,
    name: &str,
    t: &RelayTarget,
    is_static: bool,
) -> Result<()> {
    let conf = &ctx.conf;
    let mut c = Conn::open(t, &app.name, conf).await?;
    let remote_name = Conn::stream_name(t, name);
    // ngx_rtmp_relay_send_play
    let (start, duration) = if t.live {
        (-1000.0, -1000.0)
    } else {
        (
            if t.start != 0 { t.start as f64 } else { -2000.0 },
            if t.stop != 0 { (t.stop - t.start) as f64 } else { -1000.0 },
        )
    };
    c.wr.queue(&amf_cmd(
        CSID_AMF,
        c.msid,
        &[
            Amf0::str("play"),
            Amf0::Number(0.0),
            Amf0::Null,
            Amf0::str(remote_name),
            Amf0::Number(start),
            Amf0::Number(duration),
        ],
    ));
    c.wr.queue(&msg::set_buflen(c.msid, app.relay_buffer.as_millis() as u32));
    c.wr.flush().await?;
    info!("pull connected, playing '{remote_name}'");

    // Publish locally (`ngx_rtmp_relay_publish_local`).
    let id = next_session_id();
    let (tx, _rx) = mpsc::channel(1);
    let (ctl_tx, mut ctl) = mpsc::unbounded_channel();
    let close = Arc::new(Notify::new());
    let peer = Peer { id, tx, close: close.clone(), info: Arc::new(PeerInfo::new(t.url.clone(), Some(ctl_tx))) };
    let stream = ctx
        .registry
        .publish(ctx.index, app, name, &peer)
        .map_err(|e| RelayError::Local(e.to_string()))?;

    // Pulled streams are recorded too (nginx's relay session goes through
    // ngx_rtmp_publish). There is no client: only $app and $name are known.
    let mut form = crate::notify::Form::new();
    form.add("app", &app.name);
    let owner = crate::record::Owner {
        peer: None,
        vars: vec![("app", app.name.clone())],
        form,
        args: String::new(),
        exec: ctx.exec.clone(),
        netcall_timeout: conf.netcall_timeout,
    };
    let mut rec = crate::record::Recording::start(&app.recorders, ctx.exec.prefix.as_deref(), name, owner);

    let r = pull_pump(&mut c, app, &stream, is_static, &mut rec, &close, &mut ctl).await;
    drop(rec);
    drop(stream);
    ctx.registry.leave(ctx.index, &app.name, name, id);
    r
}

async fn pull_pump(
    c: &mut Conn,
    app: &AppConf,
    stream: &StreamRef,
    is_static: bool,
    rec: &mut Option<crate::record::Recording>,
    close: &Notify,
    ctl: &mut mpsc::UnboundedReceiver<Control>,
) -> Result<()> {
    let mut check =
        tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(1), Duration::from_secs(1));
    loop {
        tokio::select! {
            m = c.rd.recv() => {
                let m = m?;
                c.wr.send_replies(&mut c.rd).await?;
                if m.type_id == MSG_AGGREGATE {
                    for sub in msg::split_aggregate(&m) {
                        on_pulled(app, stream, rec, sub)?;
                    }
                } else {
                    on_pulled(app, stream, rec, m)?;
                }
            }
            _ = check.tick() => {
                if !is_static && stream.lock().unwrap().player_count() == 0 {
                    info!("no players left, closing pull");
                    return Ok(());
                }
            }
            _ = close.notified() => {
                return Err(RelayError::Local("dropped by a control request".into()));
            }
            Some(c) = ctl.recv() => match c {
                Control::Record { start, recorder, reply } => {
                    let r = match rec {
                        Some(rec) => rec.control(start, &recorder),
                        None => Err("Recorder not found".into()),
                    };
                    let _ = reply.send(r);
                }
                // Not offered to relays (see control::redirect).
                Control::Redirect { reply, .. } => {
                    let _ = reply.send(Ok(()));
                }
            },
        }
    }
}

fn on_pulled(app: &AppConf, stream: &StreamRef, rec: &mut Option<crate::record::Recording>, m: Message) -> Result<()> {
    match m.type_id {
        MSG_AUDIO | MSG_VIDEO => {
            if let Some(r) = rec {
                r.on_av(m.type_id, m.timestamp, &m.payload);
            }
            stream.lock().unwrap().on_av(m.type_id, m.timestamp, m.payload)
        }
        MSG_AMF_META | MSG_AMF3_META => {
            let body = if m.type_id == MSG_AMF3_META { m.payload.get(1..).unwrap_or_default() } else { &m.payload[..] };
            if let Ok(v) = amf::decode_all(body)
                && let Some((payload, info)) = codec::process_metadata(&v, app.meta) {
                    let ts = if app.meta == MetaMode::Copy { m.timestamp } else { 0 };
                    stream.lock().unwrap().set_meta(payload, info, ts);
                }
        }
        MSG_AMF_CMD => {
            if let Some((level, code, desc)) = status_info(&m) {
                info!("upstream status: {level} {code} {desc}");
                if level == "error" {
                    return Err(RelayError::Refused(code));
                }
            }
        }
        _ => {}
    }
    Ok(())
}
