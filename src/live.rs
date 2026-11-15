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
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};

pub type SessionId = u64;

/// Handle used to push messages to a session's writer.
#[derive(Clone)]
pub struct Peer {
    pub id: SessionId,
    pub tx: mpsc::Sender<Message>,
    pub close: Arc<Notify>,
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

#[derive(Default)]
pub struct StreamStats {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub publish_started: Option<std::time::Instant>,
}

pub struct Stream {
    pub name: String,
    app: Arc<AppConf>,
    publisher: Option<SessionId>,
    active: bool,
    players: Vec<Player>,
    /// Publisher-side last timestamps per chunk-stream index.
    pub_ts: [Option<u32>; 2],
    aac_header: Option<Bytes>,
    avc_header: Option<Bytes>,
    meta: Option<Bytes>,
    meta_version: u32,
    pub meta_info: codec::MetaInfo,
    pub stats: StreamStats,
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
}

impl Registry {
    fn key(server: usize, app: &str, name: &str) -> String {
        format!("{server}/{app}/{name}")
    }

    fn get_or_create(&self, key: &str, name: &str, app: &Arc<AppConf>) -> StreamRef {
        let mut map = self.streams.lock().unwrap();
        map.entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(Mutex::new(Stream {
                    name: name.to_string(),
                    app: app.clone(),
                    publisher: None,
                    active: false,
                    players: Vec::new(),
                    pub_ts: [None; 2],
                    aac_header: None,
                    avc_header: None,
                    meta: None,
                    meta_version: 0,
                    meta_info: Default::default(),
                    stats: Default::default(),
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
        let st = self.get_or_create(&key, name, app);
        {
            let mut s = st.lock().unwrap();
            if s.publisher.is_some() {
                return Err(LiveError::AlreadyPublishing);
            }
            s.publisher = Some(peer.id);
            s.stats.publish_started = Some(std::time::Instant::now());
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
        let st = self.get_or_create(&key, name, app);
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
            if s.publisher == Some(id) {
                s.stop();
                s.publisher = None;
                s.aac_header = None;
                s.avc_header = None;
                s.meta = None;
                s.pub_ts = [None; 2];
                s.stats.publish_started = None;
            } else {
                s.players.retain(|p| p.peer.id != id);
            }
            s.publisher.is_none() && s.players.is_empty()
        };
        if empty {
            map.remove(&key);
        }
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
    pub fn set_meta(&mut self, payload: Option<Bytes>, info: codec::MetaInfo) {
        self.meta_info = info;
        if let Some(p) = payload {
            self.meta = Some(p);
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
                        timestamp,
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
        self.stats.bytes_in += len;
        self.stats.bytes_out += len * peers;
    }
}
