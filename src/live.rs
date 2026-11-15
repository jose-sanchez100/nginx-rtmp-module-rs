//! Live stream fan-out.
//!
//! Port of `ngx_rtmp_live_module.c`. Each published stream keeps its list of
//! players together with per-player chunk-stream state, so broadcasting a
//! frame takes one lock and no per-session synchronisation.

use crate::chunk::Message;
use crate::codec;
use crate::config::AppConf;
use crate::message::{self as msg, CSID_AUDIO, CSID_VIDEO, MSG_AUDIO, MSG_VIDEO, MSID};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::{Notify, mpsc};

pub type SessionId = u64;

/// Handle used to push messages to a session's writer.
#[derive(Clone)]
pub struct Peer {
    pub id: SessionId,
    pub tx: mpsc::Sender<Message>,
    pub close: Arc<Notify>,
    pub info: Arc<PeerInfo>,
}

/// What the statistics and control modules need to know about the session
/// behind a [`Peer`].
pub struct PeerInfo {
    /// Client IP address; the upstream URL for relays.
    pub addr: String,
    pub started: Instant,
    /// Set once the client sends `connect`.
    pub connect: OnceLock<ConnectInfo>,
    /// Timestamp of the last message received (`s->current_time`).
    pub last_ts: Arc<AtomicU32>,
    /// Requests from the control module, carried out by the session task.
    pub ctl: Option<mpsc::UnboundedSender<crate::control::Control>>,
}

#[derive(Debug, Default, Clone)]
pub struct ConnectInfo {
    pub flashver: String,
    pub page_url: String,
    pub swf_url: String,
}

impl PeerInfo {
    pub fn new(addr: String, ctl: Option<mpsc::UnboundedSender<crate::control::Control>>) -> Self {
        Self { addr, started: Instant::now(), connect: OnceLock::new(), last_ts: Arc::default(), ctl }
    }
}

/// Seconds on a monotonic clock, for [`Bandwidth`].
fn now_secs() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs() + 1
}

const BANDWIDTH_INTERVAL: u64 = 10;

/// Byte counter with the rate of the last 10-second interval
/// (`ngx_rtmp_bandwidth_t`).
#[derive(Debug, Default, Clone)]
pub struct Bandwidth {
    pub bytes: u64,
    bandwidth: u64,
    intl_bytes: u64,
    intl_end: u64,
}

impl Bandwidth {
    /// `ngx_rtmp_update_bandwidth`.
    pub fn update(&mut self, bytes: u64) {
        let now = now_secs();
        if now > self.intl_end {
            self.bandwidth =
                if now > self.intl_end + BANDWIDTH_INTERVAL { 0 } else { self.intl_bytes / BANDWIDTH_INTERVAL };
            self.intl_bytes = 0;
            self.intl_end = now + BANDWIDTH_INTERVAL;
        }
        self.bytes += bytes;
        self.intl_bytes += bytes;
    }

    /// Bytes per second over the last complete interval.
    pub fn rate(&mut self) -> u64 {
        self.update(0);
        self.bandwidth
    }
}

/// Server-wide counters for the statistics page.
pub struct GlobalStats {
    pub started: Instant,
    /// Accepted connections (`ngx_rtmp_naccepted`).
    pub naccepted: AtomicU64,
    pub bw_in: Mutex<Bandwidth>,
    pub bw_out: Mutex<Bandwidth>,
}

impl Default for GlobalStats {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            naccepted: AtomicU64::new(0),
            bw_in: Default::default(),
            bw_out: Default::default(),
        }
    }
}

impl Peer {
    /// `ngx_rtmp_send_message()` with priority-based dropping: the fuller the
    /// output queue, the more frame classes get dropped (disposable first,
    /// then inter frames, then key frames). Priority 0 is never dropped
    /// early, only when the queue is completely full.
    pub fn send(&self, m: Message, prio: u8) -> bool {
        let cap = self.tx.max_capacity();
        let nmsg = cap - self.tx.capacity();
        let prio = prio.min(3) as usize;
        if prio > 0 && nmsg + prio * cap / 4 >= cap {
            return false;
        }
        self.tx.try_send(m).is_ok()
    }

    pub fn kill(&self) {
        self.close.notify_one();
    }
}

#[derive(Default, Clone, Copy)]
struct ChunkState {
    active: bool,
    timestamp: u32,
    /// Milliseconds of media dropped since last resync (`cs->dropped`).
    dropped: u32,
}

struct Player {
    peer: Peer,
    /// [0] = video (or interleaved), [1] = audio
    cs: [ChunkState; 2],
    meta_version: u32,
    paused: bool,
    ndropped: u64,
}

impl Player {
    fn reset(&mut self) {
        self.cs = Default::default();
    }
}

pub struct Stream {
    pub name: String,
    server: usize,
    app: Arc<AppConf>,
    /// When the stream was created (`stream->epoch`).
    epoch: Instant,
    publisher: Option<Peer>,
    active: bool,
    players: Vec<Player>,
    /// Publisher-side last timestamps per chunk-stream index.
    pub_ts: [Option<u32>; 2],
    aac_header: Option<Bytes>,
    avc_header: Option<Bytes>,
    meta: Option<Bytes>,
    /// Timestamp the metadata is sent with.
    meta_ts: u32,
    meta_version: u32,
    pub meta_info: codec::MetaInfo,
    /// Codec state of the current publisher.
    pub codec: codec::CodecInfo,
    pub bw_in: Bandwidth,
    pub bw_out: Bandwidth,
    pub bw_audio: Bandwidth,
    pub bw_video: Bandwidth,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum LiveError {
    #[error("Already publishing")]
    AlreadyPublishing,
    #[error("No such stream")]
    NotFound,
}

pub type StreamRef = Arc<Mutex<Stream>>;

#[derive(Default)]
pub struct Registry {
    streams: Mutex<HashMap<String, StreamRef>>,
    /// Streams with a non-static pull relay in progress (dedupe).
    pulling: Mutex<std::collections::HashSet<String>>,
    /// Connections, for user interfaces.
    pub monitor: Arc<crate::monitor::Monitor>,
    /// Totals for the statistics page.
    pub stats: GlobalStats,
}

/// Point-in-time copy of one stream.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub server: usize,
    pub app: String,
    pub name: String,
    pub publisher: Option<SessionId>,
    pub active: bool,
    pub players: Vec<SessionId>,
    pub meta: codec::MetaInfo,
}

/// One session of a stream, for the statistics page.
pub struct ClientStat {
    pub peer: Peer,
    pub publishing: bool,
    pub dropped: u64,
    /// Audio minus video timestamp sent to a player.
    pub avsync: i32,
}

/// One stream, for the statistics page.
pub struct StreamStat {
    pub name: String,
    /// Milliseconds since the stream was created.
    pub time_ms: u64,
    pub bw_in: Bandwidth,
    pub bw_out: Bandwidth,
    pub bw_audio: Bandwidth,
    pub bw_video: Bandwidth,
    pub clients: Vec<ClientStat>,
    /// Present while the stream has a publisher.
    pub codec: Option<codec::CodecInfo>,
    pub publishing: bool,
    pub active: bool,
}

impl Registry {
    fn key(server: usize, app: &str, name: &str) -> String {
        format!("{server}/{app}/{name}")
    }

    fn get_or_create(&self, server: usize, key: &str, name: &str, app: &Arc<AppConf>) -> StreamRef {
        let mut map = self.streams.lock().unwrap();
        map.entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(Mutex::new(Stream {
                    name: name.to_string(),
                    server,
                    app: app.clone(),
                    epoch: Instant::now(),
                    publisher: None,
                    active: false,
                    players: Vec::new(),
                    pub_ts: [None; 2],
                    aac_header: None,
                    avc_header: None,
                    meta: None,
                    meta_ts: 0,
                    meta_version: 0,
                    meta_info: Default::default(),
                    codec: Default::default(),
                    bw_in: Default::default(),
                    bw_out: Default::default(),
                    bw_audio: Default::default(),
                    bw_video: Default::default(),
                }))
            })
            .clone()
    }

    pub fn get(&self, server: usize, app: &str, name: &str) -> Option<StreamRef> {
        self.streams.lock().unwrap().get(&Self::key(server, app, name)).cloned()
    }

    /// Mark a pull relay as running for this stream; false if one already is.
    pub fn begin_pull(&self, server: usize, app: &str, name: &str) -> bool {
        self.pulling.lock().unwrap().insert(Self::key(server, app, name))
    }

    pub fn end_pull(&self, server: usize, app: &str, name: &str) {
        self.pulling.lock().unwrap().remove(&Self::key(server, app, name));
    }

    pub fn publish(
        &self,
        server: usize,
        app: &Arc<AppConf>,
        name: &str,
        peer: &Peer,
    ) -> Result<StreamRef, LiveError> {
        let key = Self::key(server, &app.name, name);
        let st = self.get_or_create(server, &key, name, app);
        {
            let mut s = st.lock().unwrap();
            if s.publisher.is_some() {
                return Err(LiveError::AlreadyPublishing);
            }
            s.publisher = Some(peer.clone());
        }
        Ok(st)
    }

    pub fn play(
        &self,
        server: usize,
        app: &Arc<AppConf>,
        name: &str,
        peer: &Peer,
    ) -> Result<StreamRef, LiveError> {
        let key = Self::key(server, &app.name, name);
        if !app.idle_streams && app.pulls.is_empty() {
            let map = self.streams.lock().unwrap();
            let published = map.get(&key).is_some_and(|s| s.lock().unwrap().publisher.is_some());
            if !published {
                return Err(LiveError::NotFound);
            }
        }
        let st = self.get_or_create(server, &key, name, app);
        st.lock().unwrap().players.push(Player {
            peer: peer.clone(),
            cs: Default::default(),
            meta_version: 0,
            paused: false,
            ndropped: 0,
        });
        Ok(st)
    }

    /// Remove a session (publisher or player) from its stream.
    pub fn leave(&self, server: usize, app: &str, name: &str, id: SessionId) {
        let key = Self::key(server, app, name);
        let mut map = self.streams.lock().unwrap();
        let Some(st) = map.get(&key).cloned() else { return };
        let empty = {
            let mut s = st.lock().unwrap();
            if s.publisher.as_ref().is_some_and(|p| p.id == id) {
                s.stop();
                s.publisher = None;
                s.aac_header = None;
                s.avc_header = None;
                s.meta = None;
                s.pub_ts = [None; 2];
                s.codec = Default::default();
            } else {
                s.players.retain(|p| p.peer.id != id);
            }
            s.publisher.is_none() && s.players.is_empty()
        };
        if empty {
            map.remove(&key);
        }
    }

    /// Snapshot of every stream, sorted by server, application and name.
    pub fn streams(&self) -> Vec<StreamInfo> {
        let map = self.streams.lock().unwrap();
        let mut v: Vec<StreamInfo> = map
            .iter()
            .map(|(k, s)| {
                let s = s.lock().unwrap();
                StreamInfo {
                    server: k.split('/').next().and_then(|n| n.parse().ok()).unwrap_or(0),
                    app: s.app.name.clone(),
                    name: s.name.clone(),
                    publisher: s.publisher.as_ref().map(|p| p.id),
                    active: s.active,
                    players: s.players.iter().map(|p| p.peer.id).collect(),
                    meta: s.meta_info.clone(),
                }
            })
            .collect();
        v.sort_by(|a, b| (a.server, &a.app, &a.name).cmp(&(b.server, &b.app, &b.name)));
        v
    }

    /// Streams of one application, sorted by name (`ngx_rtmp_stat_live`).
    pub fn stat(&self, server: usize, app: &str) -> Vec<StreamStat> {
        let streams: Vec<StreamRef> = self.streams.lock().unwrap().values().cloned().collect();
        let mut v: Vec<StreamStat> = streams
            .iter()
            .filter_map(|s| {
                let mut guard = s.lock().unwrap();
                let s = &mut *guard;
                if s.server != server || s.app.name != app {
                    return None;
                }
                let mut clients = Vec::new();
                if let Some(p) = &s.publisher {
                    clients.push(ClientStat { peer: p.clone(), publishing: true, dropped: 0, avsync: 0 });
                }
                let interleave = s.app.interleave;
                for p in &s.players {
                    clients.push(ClientStat {
                        peer: p.peer.clone(),
                        publishing: false,
                        dropped: p.ndropped,
                        avsync: if interleave { 0 } else { p.cs[1].timestamp.wrapping_sub(p.cs[0].timestamp) as i32 },
                    });
                }
                for bw in [&mut s.bw_in, &mut s.bw_out, &mut s.bw_audio, &mut s.bw_video] {
                    bw.update(0);
                }
                Some(StreamStat {
                    name: s.name.clone(),
                    time_ms: s.epoch.elapsed().as_millis() as u64,
                    bw_in: s.bw_in.clone(),
                    bw_out: s.bw_out.clone(),
                    bw_audio: s.bw_audio.clone(),
                    bw_video: s.bw_video.clone(),
                    clients,
                    codec: s.publisher.is_some().then(|| s.codec.clone()),
                    publishing: s.publisher.is_some(),
                    active: s.active,
                })
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Publisher and players of the streams of one application, or of
    /// the stream `name` only (`ngx_rtmp_control_walk_app`). The flag tells
    /// the publisher.
    pub fn sessions(&self, server: usize, app: &str, name: Option<&str>) -> Vec<(Peer, bool)> {
        let streams: Vec<StreamRef> = match name {
            Some(n) => self.get(server, app, n).into_iter().collect(),
            None => self.streams.lock().unwrap().values().cloned().collect(),
        };
        let mut out = Vec::new();
        for s in streams {
            let s = s.lock().unwrap();
            if s.server != server || s.app.name != app {
                continue;
            }
            out.extend(s.publisher.iter().map(|p| (p.clone(), true)));
            out.extend(s.players.iter().map(|p| (p.peer.clone(), false)));
        }
        out
    }

    /// Snapshot for statistics: (key, publisher present, player count).
    pub fn list(&self) -> Vec<(String, bool, usize)> {
        let map = self.streams.lock().unwrap();
        map.iter()
            .map(|(k, s)| {
                let s = s.lock().unwrap();
                (k.clone(), s.publisher.is_some(), s.players.len())
            })
            .collect()
    }
}

impl Stream {
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn has_publisher(&self) -> bool {
        self.publisher.is_some()
    }

    pub fn player_count(&self) -> usize {
        self.players.len()
    }

    /// Disconnect every player (relay "play disconnect orphan").
    pub fn kill_players(&self) {
        for p in &self.players {
            p.peer.kill();
        }
    }

    /// Publisher became active (`ngx_rtmp_live_start`).
    fn start(&mut self) {
        if self.active {
            return;
        }
        self.active = true;
        let app = self.app.clone();
        for p in &mut self.players {
            let mut ok = p.peer.send(msg::stream_begin(MSID), 0);
            if app.play_restart {
                ok &= p.peer.send(msg::status("NetStream.Play.Start", "status", "Start live"), 0);
            }
            if app.publish_notify {
                ok &= p.peer.send(
                    msg::status("NetStream.Play.PublishNotify", "status", "Start publishing"),
                    0,
                );
            }
            if !ok {
                p.peer.kill();
            }
            p.reset();
        }
    }

    /// Publisher went away (`ngx_rtmp_live_stop`).
    fn stop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let app = self.app.clone();
        for p in &mut self.players {
            let mut ok = p.peer.send(msg::stream_eof(MSID), 0);
            if app.play_restart {
                ok &= p.peer.send(msg::status("NetStream.Play.Stop", "status", "Stop live"), 0);
            }
            if app.publish_notify {
                ok &= p.peer.send(
                    msg::status("NetStream.Play.UnpublishNotify", "status", "Stop publishing"),
                    0,
                );
            }
            if !ok {
                p.peer.kill();
            }
            p.reset();
        }
    }

    pub fn set_paused(&mut self, id: SessionId, paused: bool) {
        if let Some(p) = self.players.iter_mut().find(|p| p.peer.id == id) {
            p.paused = paused;
            if !paused {
                p.reset();
            }
        }
    }

    /// Store metadata to be (re)sent to every player before its next frame.
    pub fn set_meta(&mut self, payload: Option<Bytes>, info: codec::MetaInfo, timestamp: u32) {
        self.codec.on_meta(&info);
        self.meta_info = info;
        if let Some(p) = payload {
            self.meta = Some(p);
            self.meta_ts = timestamp;
            self.meta_version = self.meta_version.wrapping_add(1).max(1);
        }
    }

    /// Broadcast an audio/video message from the publisher (`ngx_rtmp_live_av`).
    pub fn on_av(&mut self, type_id: u8, timestamp: u32, payload: Bytes) {
        if payload.is_empty() {
            return;
        }
        if !self.active {
            self.start();
        }
        let app = self.app.clone();
        let video = type_id == MSG_VIDEO;
        self.codec.on_av(video, &payload);

        // Codec module: remember sequence headers for late joiners.
        let mut mandatory = false;
        let mut prio = if video { codec::video_frame_type(&payload) } else { 0 };
        if video && codec::is_video_header(&payload) {
            self.avc_header = Some(payload.clone());
            mandatory = true;
            prio = 0;
        } else if !video && codec::is_audio_header(&payload) {
            self.aac_header = Some(payload.clone());
            mandatory = true;
            prio = 0;
        }

        let csidx = if app.interleave || video { 0 } else { 1 };
        let csid = [CSID_VIDEO, CSID_AUDIO][csidx];
        let delta = timestamp.wrapping_sub(self.pub_ts[csidx].unwrap_or(timestamp));
        self.pub_ts[csidx] = Some(timestamp);

        let (header, coheader) = if video {
            (self.avc_header.clone(), if app.interleave { self.aac_header.clone() } else { None })
        } else {
            (self.aac_header.clone(), if app.interleave { self.avc_header.clone() } else { None })
        };
        let other_type = if video { MSG_AUDIO } else { MSG_VIDEO };
        let mk = |t: u8, p: Bytes| Message { csid, timestamp, type_id: t, msid: MSID, payload: p };
        let sync_ms = app.sync.as_millis() as u32;

        let mut peers = 0u64;
        for p in self.players.iter_mut() {
            if p.paused {
                continue;
            }
            if let Some(meta) = &self.meta
                && p.meta_version != self.meta_version {
                    let m = Message {
                        csid: msg::CSID_AMF,
                        timestamp: self.meta_ts,
                        type_id: msg::MSG_AMF_META,
                        msid: MSID,
                        payload: meta.clone(),
                    };
                    if p.peer.send(m, 0) {
                        p.meta_version = self.meta_version;
                    }
                }

            let cs_audio_active = p.cs[1].active;
            let cs_video_active = p.cs[0].active;
            let cs = &mut p.cs[csidx];

            if cs.active && sync_ms > 0 && cs.dropped > sync_ms {
                cs.active = false;
                cs.dropped = 0;
            }

            if !cs.active {
                if mandatory {
                    continue;
                }
                if app.wait_video && !video && !cs_video_active {
                    continue;
                }
                if app.wait_key && prio != codec::FRAME_KEY && (app.interleave || video) {
                    continue;
                }
                let dummy_audio = app.wait_video && video && !cs_audio_active;

                if header.is_some() || coheader.is_some() {
                    // Absolute codec header(s), then fall through to the frame.
                    if let Some(h) = &header
                        && !p.peer.send(mk(type_id, h.clone()), 0) {
                            continue;
                        }
                    if let Some(h) = &coheader {
                        if !p.peer.send(mk(other_type, h.clone()), 0) {
                            continue;
                        }
                    } else if dummy_audio {
                        p.peer.send(mk(MSG_AUDIO, Bytes::new()), 0);
                    }
                    cs.timestamp = timestamp;
                    cs.active = true;
                } else {
                    if !p.peer.send(mk(type_id, payload.clone()), prio) {
                        continue;
                    }
                    cs.timestamp = timestamp;
                    cs.active = true;
                    peers += 1;
                    if dummy_audio {
                        p.peer.send(mk(MSG_AUDIO, Bytes::new()), 0);
                    }
                    continue;
                }
            }

            if !p.peer.send(mk(type_id, payload.clone()), prio) {
                p.ndropped += 1;
                cs.dropped = cs.dropped.saturating_add(delta);
                if mandatory {
                    p.peer.kill();
                }
                continue;
            }
            cs.timestamp = timestamp;
            peers += 1;
        }

        let len = payload.len() as u64;
        self.bw_in.update(len);
        self.bw_out.update(len * peers);
        if video { &mut self.bw_video } else { &mut self.bw_audio }.update(len);
    }
}
