//! FLV recording of published streams.
//!
//! Port of `ngx_rtmp_record_module.c`. Every application may record with
//! its own `record*` settings (the unnamed recorder) and with any number
//! of `recorder NAME { ... }` blocks. Each recorder writes its own file per
//! published stream. When a file is closed, `exec_record_done` and
//! `on_record_done` run.
//!
//! A [`Recording`] belongs to whoever publishes the stream locally: a
//! client session or a `pull` relay. It gets every audio/video message
//! after the live module and writes plain, unbuffered FLV, so the file on
//! disk is always complete up to the last frame, as in nginx.

use crate::codec;
use crate::exec::{ExecConf, MainExec, Vars};
use crate::live::Peer;
use crate::message::{self as msg, MSG_AUDIO, MSG_VIDEO};
use crate::notify::{self, Form, Method, NotifyUrl, Reply};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{Instrument, debug, error, info};

pub const OFF: u8 = 0x01;
pub const AUDIO: u8 = 0x02;
pub const VIDEO: u8 = 0x04;
pub const KEYFRAMES: u8 = 0x08;
pub const MANUAL: u8 = 0x10;

/// Parse the `record` values (`ngx_rtmp_record_mask`).
pub fn parse_flag(v: &str) -> Option<u8> {
    Some(match v {
        "off" => OFF,
        "all" => AUDIO | VIDEO,
        "audio" => AUDIO,
        "video" => VIDEO,
        "keyframes" => KEYFRAMES,
        "manual" => MANUAL,
        _ => return None,
    })
}

/// `record*` directives at one level (`ngx_rtmp_record_app_conf_t`).
#[derive(Debug, Clone)]
pub struct RecordConf {
    /// `record`: OR of the flags above; 0 means not recording.
    pub flags: u8,
    pub path: String,
    pub suffix: String,
    pub unique: bool,
    pub append: bool,
    pub lock: bool,
    pub notify: bool,
    /// 0 = no limit.
    pub max_size: u64,
    /// 0 = no limit.
    pub max_frames: u64,
    pub interval: Option<Duration>,
}

impl Default for RecordConf {
    fn default() -> Self {
        Self {
            flags: 0,
            path: String::new(),
            suffix: ".flv".into(),
            unique: false,
            append: false,
            lock: false,
            notify: false,
            max_size: 0,
            max_frames: 0,
            interval: None,
        }
    }
}

/// One recorder of an application: the unnamed one (`id` empty) or a
/// `recorder` block, with the settings it was configured with.
#[derive(Debug)]
pub struct Recorder {
    pub id: String,
    pub conf: RecordConf,
    pub exec_done: Vec<Arc<ExecConf>>,
    pub on_done: Option<Arc<NotifyUrl>>,
    pub method: Method,
}

impl Recorder {
    /// A recorder from its (application or `recorder` block) settings, or
    /// `None` when it does not record.
    pub fn new(id: &str, app: &crate::config::AppConf) -> Option<Self> {
        let conf = &app.record;
        if conf.flags == 0 || conf.flags & OFF != 0 {
            return None;
        }
        Some(Self {
            id: id.to_string(),
            conf: conf.clone(),
            exec_done: app.exec.record_done.clone(),
            on_done: app.notify.on_record_done.clone(),
            method: app.notify.method,
        })
    }
}

/// Who published the stream, for the `record_done` hooks and the
/// `NetStream.Record.*` status messages.
pub struct Owner {
    /// The publishing client (`None` for relays): receives the status
    /// messages of `record_notify on`.
    pub peer: Option<Peer>,
    /// Session variables for `exec_record_done`.
    pub vars: Vars,
    /// Session parameters for `on_record_done`.
    pub form: Form,
    pub args: String,
    pub exec: Arc<MainExec>,
    pub netcall_timeout: Duration,
}

impl Owner {
    fn status(&self, code: &str, level: &str, rec: &Recorder) {
        if let Some(p) = &self.peer {
            p.send(msg::status(code, level, &rec.id), 0);
        }
    }

    /// `ngx_rtmp_record_done`: the exec and notify modules' handlers.
    fn done(&self, rec: &Recorder, name: &str, path: &str) {
        crate::exec::record_done(&self.exec, &rec.exec_done, self.vars.clone(), name, &self.args, &rec.id, path);
        let Some(url) = rec.on_done.clone() else { return };
        info!("notify: record_done recorder={} path='{path}' url='{}'", rec.id, url.raw);
        let mut f = self.form.clone();
        f.add("call", "record_done").add("recorder", &rec.id).add("name", name).add("path", path).raw(&self.args);
        let (method, timeout) = (rec.method, self.netcall_timeout);
        tokio::spawn(
            async move {
                if let Reply::Error(e) = notify::call(&url, method, &f, timeout).await {
                    debug!("notify: record_done failed: {e}");
                }
            }
            .in_current_span(),
        );
    }
}

/// Last codec state of the stream (what `ngx_rtmp_codec_module` keeps).
#[derive(Default)]
struct Codec {
    aac_header: Option<Vec<u8>>,
    avc_header: Option<Vec<u8>>,
    audio_codec_id: u8,
    video_codec_id: u8,
}

/// One recorder's file (`ngx_rtmp_record_rec_ctx_t`).
struct Node {
    rec: Arc<Recorder>,
    file: Option<File>,
    path: String,
    offset: u64,
    /// When the file was opened: drives `record_interval`, and names it
    /// (`record_unique`, `strftime` suffix).
    opened: Instant,
    opened_wall: chrono::DateTime<chrono::Local>,
    nframes: u64,
    epoch: u32,
    time_shift: u32,
    failed: bool,
    initialized: bool,
    aac_header_sent: bool,
    avc_header_sent: bool,
    video_key_sent: bool,
    audio: bool,
    video: bool,
}

/// The recordings of one published stream.
pub struct Recording {
    name: String,
    prefix: Option<PathBuf>,
    owner: Owner,
    codec: Codec,
    nodes: Vec<Node>,
}

impl Recording {
    /// `ngx_rtmp_record_publish`: open the files of every automatic
    /// recorder. `None` when the application records nothing.
    pub fn start(recorders: &[Arc<Recorder>], prefix: Option<&Path>, name: &str, owner: Owner) -> Option<Self> {
        if recorders.is_empty() {
            return None;
        }
        debug!("record: publish {} nodes", recorders.len());
        let mut r = Recording {
            name: clean_name(name).to_string(),
            prefix: prefix.map(Path::to_path_buf),
            owner,
            codec: Codec::default(),
            nodes: recorders.iter().map(|rec| Node::new(rec.clone())).collect(),
        };
        for i in 0..r.nodes.len() {
            if r.nodes[i].rec.conf.flags & MANUAL == 0 {
                r.open(i);
            }
        }
        Some(r)
    }

    /// `ngx_rtmp_record_av`: one audio or video message from the publisher.
    pub fn on_av(&mut self, type_id: u8, timestamp: u32, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        // ngx_rtmp_codec_av runs first.
        if type_id == MSG_VIDEO {
            self.codec.video_codec_id = codec::video_codec_id(payload);
            if codec::is_video_header(payload) {
                self.codec.avc_header = Some(payload.to_vec());
            }
        } else {
            self.codec.audio_codec_id = codec::audio_codec_id(payload);
            if codec::is_audio_header(payload) {
                self.codec.aac_header = Some(payload.to_vec());
            }
        }
        for i in 0..self.nodes.len() {
            self.node_av(i, type_id, timestamp, payload);
        }
    }

    /// `ngx_rtmp_record_stop`: close every open file.
    pub fn stop(&mut self) {
        debug!("record: stop");
        for i in 0..self.nodes.len() {
            self.close(i);
        }
    }

    fn make_path(&self, n: &Node) -> String {
        let c = &n.rec.conf;
        let mut file = escape(&self.name);
        if c.unique {
            file.push_str(&format!("-{}", n.opened_wall.timestamp()));
        }
        if c.suffix.contains('%') {
            use std::fmt::Write as _;
            let mut s = String::new();
            if write!(s, "{}", n.opened_wall.format(&c.suffix)).is_err() {
                s = c.suffix.clone();
            }
            file.push_str(&s);
        } else {
            file.push_str(&c.suffix);
        }
        // Like nginx the name is appended to record_path; an empty
        // record_path writes into the prefix instead of the root.
        let dir = crate::access_log::resolve_path(self.prefix.as_deref(), &c.path);
        let dir = if c.path.is_empty() { self.prefix.clone().unwrap_or_default() } else { dir };
        dir.join(file).display().to_string()
    }

    /// `ngx_rtmp_record_node_open`.
    fn open(&mut self, i: usize) {
        if self.nodes[i].file.is_some() {
            return;
        }
        let rec = self.nodes[i].rec.clone();
        let mut n = Node::new(rec.clone());
        let path = self.make_path(&n);
        debug!("record: {} opening '{path}'", rec.id);
        let c = &rec.conf;
        if let Some(dir) = Path::new(&path).parent()
            && !dir.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(dir);
        }
        let opened = if c.append {
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)
        } else {
            OpenOptions::new().write(true).create(true).truncate(true).open(&path)
        };
        n.path = path;
        let mut file = match opened {
            Ok(f) => f,
            Err(e) => {
                error!("record: {} failed to open file '{}': {e}", rec.id, n.path);
                n.failed = true;
                self.nodes[i] = n;
                self.notify_error(i);
                return;
            }
        };
        if c.lock {
            lock(&file, &rec.id);
        }
        info!("record: {} opened '{}'", if rec.id.is_empty() { "-" } else { &rec.id }, n.path);
        if c.notify {
            self.owner.status("NetStream.Record.Start", "status", &rec);
        }
        if c.append {
            let (offset, ts) = append_point(&mut file).unwrap_or_else(|e| {
                error!("record: {} append: {e}", rec.id);
                (file.seek(SeekFrom::End(0)).unwrap_or(0), 0)
            });
            debug!("record: append offset={offset}, time={ts}");
            n.offset = offset;
            n.time_shift = ts;
            if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                error!("record: {} seek failed: {e}", rec.id);
            }
        }
        n.file = Some(file);
        self.nodes[i] = n;
    }

    /// `ngx_rtmp_record_node_close`.
    fn close(&mut self, i: usize) {
        let n = &mut self.nodes[i];
        let Some(mut file) = n.file.take() else { return };
        if n.initialized {
            let av = if n.video { 0x01 } else { 0 } | if n.audio { 0x04 } else { 0 };
            if let Err(e) = file.seek(SeekFrom::Start(4)).and_then(|_| file.write_all(&[av])) {
                error!("record: {} error writing av mask: {e}", n.rec.id);
            }
        }
        drop(file);
        let (rec, path) = (n.rec.clone(), n.path.clone());
        info!("record: {} closed '{path}'", if rec.id.is_empty() { "-" } else { &rec.id });
        if rec.conf.notify {
            self.owner.status("NetStream.Record.Stop", "status", &rec);
        }
        self.owner.done(&rec, &self.name, &path);
    }

    fn notify_error(&mut self, i: usize) {
        let n = &mut self.nodes[i];
        n.failed = true;
        if n.rec.conf.notify {
            let rec = n.rec.clone();
            self.owner.status("NetStream.Record.Failed", "error", &rec);
        }
    }

    /// `ngx_rtmp_record_node_av`.
    fn node_av(&mut self, i: usize, type_id: u8, timestamp: u32, payload: &[u8]) {
        let video = type_id == MSG_VIDEO;
        let flags = self.nodes[i].rec.conf.flags;
        let keyframe = video && codec::video_frame_type(payload) == codec::FRAME_KEY;
        let brkframe = if video { keyframe } else { flags & VIDEO == 0 };

        if brkframe && flags & MANUAL == 0 {
            if let Some(interval) = self.nodes[i].rec.conf.interval {
                if self.nodes[i].opened.elapsed() > interval {
                    self.close(i);
                    self.open(i);
                }
            } else if !self.nodes[i].failed {
                self.open(i);
            }
        }

        let n = &self.nodes[i];
        if flags & MANUAL != 0 && !brkframe && n.nframes == 0 {
            return;
        }
        if n.file.is_none() {
            return;
        }
        if !video && flags & AUDIO == 0 {
            return;
        }
        if video && flags & VIDEO == 0 && (flags & KEYFRAMES == 0 || !keyframe) {
            return;
        }

        if !n.initialized {
            let n = &mut self.nodes[i];
            n.initialized = true;
            n.epoch = timestamp.wrapping_sub(n.time_shift);
            if n.offset == 0 && !self.write(i, &FLV_HEADER) {
                self.close(i);
                return;
            }
        }

        if !self.nodes[i].aac_header_sent
            && flags & AUDIO != 0
            && let Some(h) = self.codec.aac_header.clone()
        {
            debug!("record: {} writing AAC header", self.nodes[i].rec.id);
            if !self.write_frame(i, MSG_AUDIO, timestamp, &h, false) {
                return;
            }
            self.nodes[i].aac_header_sent = true;
            // nginx writes the header a second time when this message is
            // the header itself.
            if !video && codec::is_audio_header(payload) {
                return;
            }
        }
        if !self.nodes[i].avc_header_sent
            && flags & (VIDEO | KEYFRAMES) != 0
            && let Some(h) = self.codec.avc_header.clone()
        {
            debug!("record: {} writing AVC header", self.nodes[i].rec.id);
            if !self.write_frame(i, MSG_VIDEO, timestamp, &h, false) {
                return;
            }
            self.nodes[i].avc_header_sent = true;
            if video && codec::is_video_header(payload) {
                return;
            }
        }

        let n = &mut self.nodes[i];
        if video {
            if self.codec.video_codec_id == codec::VIDEO_H264 && !n.avc_header_sent {
                return; // skipping until H264 header
            }
            if keyframe && !codec::is_video_header(payload) {
                n.video_key_sent = true;
            }
            if !n.video_key_sent {
                return; // skipping until keyframe
            }
        } else if self.codec.audio_codec_id == codec::AUDIO_AAC && !n.aac_header_sent {
            return; // skipping until AAC header
        }
        self.write_frame(i, type_id, timestamp, payload, true);
    }

    /// Raw write at the current offset; on error the file is abandoned
    /// (`NetStream.Record.Failed`, no `record_done`).
    fn write(&mut self, i: usize, data: &[u8]) -> bool {
        let n = &mut self.nodes[i];
        let Some(file) = &mut n.file else { return false };
        match file.write_all(data) {
            Ok(()) => {
                n.offset += data.len() as u64;
                true
            }
            Err(e) => {
                error!("record: {} write failed '{}': {e}", n.rec.id, n.path);
                n.file = None;
                self.notify_error(i);
                false
            }
        }
    }

    /// `ngx_rtmp_record_write_frame`: one FLV tag, then the size limits.
    fn write_frame(&mut self, i: usize, type_id: u8, timestamp: u32, payload: &[u8], count: bool) -> bool {
        let n = &mut self.nodes[i];
        if type_id == MSG_VIDEO {
            n.video = true;
        } else {
            n.audio = true;
        }
        let mut ts = timestamp.wrapping_sub(n.epoch);
        if (ts as i32) < 0 {
            debug!("record: {} cut timestamp={}", n.rec.id, ts as i32);
            ts = 0;
        }
        let len = payload.len() as u32;
        let mut tag = Vec::with_capacity(11 + payload.len() + 4);
        tag.push(type_id);
        tag.extend_from_slice(&len.to_be_bytes()[1..]);
        tag.extend_from_slice(&ts.to_be_bytes()[1..]);
        tag.push((ts >> 24) as u8);
        tag.extend_from_slice(&[0, 0, 0]);
        tag.extend_from_slice(payload);
        tag.extend_from_slice(&(11 + len).to_be_bytes());
        if !self.write(i, &tag) {
            return false;
        }
        let n = &mut self.nodes[i];
        if count {
            n.nframes += 1;
        }
        let c = &n.rec.conf;
        if (c.max_size > 0 && n.offset >= c.max_size) || (c.max_frames > 0 && n.nframes >= c.max_frames) {
            self.close(i);
        }
        true
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Node {
    fn new(rec: Arc<Recorder>) -> Self {
        Node {
            rec,
            file: None,
            path: String::new(),
            offset: 0,
            opened: Instant::now(),
            opened_wall: chrono::Local::now(),
            nframes: 0,
            epoch: 0,
            time_shift: 0,
            failed: false,
            initialized: false,
            aac_header_sent: false,
            avc_header_sent: false,
            video_key_sent: false,
            audio: false,
            video: false,
        }
    }
}

/// `record_lock`: an exclusive lock while the file is being written, so
/// other programs can tell it is not finished. Like nginx, nothing on
/// Windows (a lock there would also stop readers).
#[cfg(not(windows))]
fn lock(file: &File, id: &str) {
    if let Err(e) = file.try_lock() {
        error!("record: {id} lock failed: {e}");
    }
}

#[cfg(windows)]
fn lock(_file: &File, _id: &str) {}

/// FLV header with audio and video flags, plus PreviousTagSize0.
const FLV_HEADER: [u8; 13] = [b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0];

/// `record_append`: where to continue an existing FLV file and the
/// timestamp of its last tag. An empty or unreadable tail starts over at 0.
fn append_point(f: &mut File) -> std::io::Result<(u64, u32)> {
    let size = f.seek(SeekFrom::End(0))?;
    if size < 4 {
        return Ok((size, 0));
    }
    let mut b = [0u8; 8];
    f.seek(SeekFrom::Start(size - 4))?;
    f.read_exact(&mut b[..4])?;
    let tag_size = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
    if tag_size == 0 || tag_size + 4 > size {
        return Ok((0, 0));
    }
    f.seek(SeekFrom::Start(size - tag_size - 4))?;
    f.read_exact(&mut b)?;
    let mlen = u32::from_be_bytes([0, b[1], b[2], b[3]]) as u64;
    if tag_size != mlen + 11 {
        error!("record: tag size mismatch: tag_size={tag_size}, mlen={mlen}");
        return Ok((size, 0));
    }
    Ok((size, u32::from_be_bytes([b[7], b[4], b[5], b[6]])))
}

/// The stream name is cut at the first `/../` (`ngx_rtmp_record_publish`).
fn clean_name(name: &str) -> &str {
    let b = name.as_bytes();
    let sep = |c: u8| c == b'/' || c == b'\\';
    (0..b.len())
        .find(|&i| i + 3 < b.len() && sep(b[i]) && b[i + 1] == b'.' && b[i + 2] == b'.' && sep(b[i + 3]))
        .map_or(name, |i| &name[..i])
}

/// `ngx_escape_uri(NGX_ESCAPE_URI_COMPONENT)`: everything but unreserved
/// characters is percent-encoded, so a stream name never leaves the
/// recording directory.
fn escape(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(clean_name("a/../b"), "a");
        assert_eq!(clean_name("a..b"), "a..b");
        assert_eq!(escape("cam 1/x?y"), "cam%201%2Fx%3Fy");
        assert_eq!(escape("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn flags() {
        assert_eq!(parse_flag("all"), Some(AUDIO | VIDEO));
        assert_eq!(parse_flag("keyframes"), Some(KEYFRAMES));
        assert_eq!(parse_flag("nope"), None);
    }
}
