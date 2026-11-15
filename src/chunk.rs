//! RTMP chunk stream de/multiplexing.
//!
//! Port of the chunk parser in `ngx_rtmp_recv()` (`ngx_rtmp_handler.c`) and the
//! chunk header writer in `ngx_rtmp_prepare_message()`.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::collections::HashMap;

pub const DEFAULT_CHUNK_SIZE: usize = 128;
/// nginx-rtmp caps peer chunk size requests to this value.
pub const MAX_CHUNK_SIZE: usize = 10_485_760;
const EXT_TS: u32 = 0x00ff_ffff;

/// A complete, reassembled RTMP message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub csid: u32,
    pub timestamp: u32,
    pub type_id: u8,
    pub msid: u32,
    pub payload: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error("too big message: {0} > {1}")]
    TooBig(u32, usize),
    #[error("too many chunk streams (limit {0})")]
    TooManyStreams(usize),
    #[error("chunk continuation (fmt={fmt}) on csid {csid} without prior header")]
    NoPriorHeader { fmt: u8, csid: u32 },
}

#[derive(Default)]
struct InStream {
    timestamp: u32,
    mlen: u32,
    type_id: u8,
    msid: u32,
    /// Timestamp delta from the last fmt 1/2 header (nginx `st->dtime`).
    dtime: u32,
    /// Whether fmt 3 chunks on this stream carry an extended timestamp.
    ext: bool,
    have_header: bool,
    buf: BytesMut,
}

/// Incremental decoder. Feed bytes into a `BytesMut` and call [`decode`]
/// until it returns `Ok(None)`.
///
/// [`decode`]: ChunkDecoder::decode
pub struct ChunkDecoder {
    chunk_size: usize,
    max_message: usize,
    max_streams: usize,
    publish_time_fix: bool,
    streams: HashMap<u32, InStream>,
}

impl ChunkDecoder {
    pub fn new(max_message: usize, max_streams: usize, publish_time_fix: bool) -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_message,
            max_streams,
            publish_time_fix,
            streams: HashMap::new(),
        }
    }

    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size.clamp(1, MAX_CHUNK_SIZE);
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Handle an Abort message: drop the partially received message.
    pub fn abort(&mut self, csid: u32) {
        if let Some(st) = self.streams.get_mut(&csid) {
            st.buf.clear();
        }
    }

    /// Parse one chunk from `input`. Returns a message when one completes.
    /// Bytes are consumed only when a whole chunk is available.
    pub fn decode(&mut self, input: &mut BytesMut) -> Result<Option<Message>, ChunkError> {
        loop {
            match self.decode_chunk(input)? {
                Step::NeedMore => return Ok(None),
                Step::Partial => continue,
                Step::Done(m) => return Ok(Some(m)),
            }
        }
    }

    fn decode_chunk(&mut self, input: &mut BytesMut) -> Result<Step, ChunkError> {
        let b = &input[..];
        if b.is_empty() {
            return Ok(Step::NeedMore);
        }
        let mut p = 0usize;
        let fmt = b[0] >> 6;
        let mut csid = (b[0] & 0x3f) as u32;
        p += 1;
        if csid == 0 {
            if b.len() < p + 1 {
                return Ok(Step::NeedMore);
            }
            csid = 64 + b[p] as u32;
            p += 1;
        } else if csid == 1 {
            if b.len() < p + 2 {
                return Ok(Step::NeedMore);
            }
            csid = 64 + b[p] as u32 + 256 * b[p + 1] as u32;
            p += 2;
        }

        if !self.streams.contains_key(&csid) && self.streams.len() >= self.max_streams {
            return Err(ChunkError::TooManyStreams(self.max_streams));
        }
        let st = self.streams.entry(csid).or_default();
        if fmt != 0 && !st.have_header {
            return Err(ChunkError::NoPriorHeader { fmt, csid });
        }

        let hdr_len = match fmt {
            0 => 11,
            1 => 7,
            2 => 3,
            _ => 0,
        };
        if b.len() < p + hdr_len {
            return Ok(Step::NeedMore);
        }

        let mut ext = st.ext;
        let mut timestamp = st.dtime;
        let (mut mlen, mut type_id, mut msid) = (st.mlen, st.type_id, st.msid);
        if fmt <= 2 {
            timestamp = be24(&b[p..]);
            p += 3;
            ext = timestamp == EXT_TS;
            if fmt <= 1 {
                mlen = be24(&b[p..]);
                type_id = b[p + 3];
                p += 4;
                if fmt == 0 {
                    msid = u32::from_le_bytes(b[p..p + 4].try_into().unwrap());
                    p += 4;
                }
            }
        }
        if ext {
            if b.len() < p + 4 {
                return Ok(Step::NeedMore);
            }
            timestamp = u32::from_be_bytes(b[p..p + 4].try_into().unwrap());
            p += 4;
        }

        if mlen as usize > self.max_message {
            return Err(ChunkError::TooBig(mlen, self.max_message));
        }

        let starting = st.buf.is_empty();
        let have = st.buf.len();
        let remaining = mlen as usize - have.min(mlen as usize);
        let size = remaining.min(self.chunk_size);
        if b.len() < p + size {
            return Ok(Step::NeedMore);
        }

        // Whole chunk available: commit header state.
        if starting {
            // Type-3 messages should never carry an extended timestamp, but in
            // practice some encoders send one (nginx: publish_time_fix).
            st.ext = ext && self.publish_time_fix;
            if fmt == 0 {
                st.timestamp = timestamp;
                st.dtime = 0;
            } else {
                st.dtime = timestamp;
            }
            st.mlen = mlen;
            st.type_id = type_id;
            st.msid = msid;
            st.have_header = true;
            st.buf.reserve(mlen as usize);
        }

        input.advance(p);
        st.buf.extend_from_slice(&input[..size]);
        input.advance(size);

        if st.buf.len() < st.mlen as usize {
            return Ok(Step::Partial);
        }
        st.timestamp = st.timestamp.wrapping_add(st.dtime);
        let payload = st.buf.split().freeze();
        Ok(Step::Done(Message {
            csid,
            timestamp: st.timestamp,
            type_id: st.type_id,
            msid: st.msid,
            payload,
        }))
    }
}

enum Step {
    NeedMore,
    Partial,
    Done(Message),
}

fn be24(b: &[u8]) -> u32 {
    (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32
}

fn put_be24(out: &mut BytesMut, v: u32) {
    out.put_u8((v >> 16) as u8);
    out.put_u8((v >> 8) as u8);
    out.put_u8(v as u8);
}

#[derive(Clone, Copy)]
struct OutState {
    timestamp: u32,
    delta: u32,
    mlen: u32,
    type_id: u8,
    msid: u32,
    /// Whether the last header on this csid used an extended timestamp; the
    /// peer applies it to following fmt 3 chunks.
    ext: bool,
}

/// Chunk encoder with header compression.
pub struct ChunkEncoder {
    chunk_size: usize,
    last: HashMap<u32, OutState>,
}

impl Default for ChunkEncoder {
    fn default() -> Self {
        Self { chunk_size: DEFAULT_CHUNK_SIZE, last: HashMap::new() }
    }
}

impl ChunkEncoder {
    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size.clamp(1, MAX_CHUNK_SIZE);
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Forget header state so the next message on every csid uses fmt 0.
    pub fn reset(&mut self) {
        self.last.clear();
    }

    pub fn encode(&mut self, m: &Message, out: &mut BytesMut) {
        let mlen = m.payload.len() as u32;
        let prev = self.last.get(&m.csid).copied();

        // Pick the most compact header that is unambiguous.
        let (fmt, tsfield) = match prev {
            Some(l) if l.msid == m.msid && m.timestamp >= l.timestamp => {
                let delta = m.timestamp - l.timestamp;
                if l.type_id == m.type_id && l.mlen == mlen {
                    if delta == l.delta && l.ext == (delta >= EXT_TS) {
                        (3u8, delta)
                    } else {
                        (2u8, delta)
                    }
                } else {
                    (1u8, delta)
                }
            }
            _ => (0u8, m.timestamp),
        };
        let delta = if fmt == 0 { 0 } else { tsfield };
        let ext = tsfield >= EXT_TS;
        self.last.insert(
            m.csid,
            OutState { timestamp: m.timestamp, delta, mlen, type_id: m.type_id, msid: m.msid, ext },
        );

        out.reserve(mlen as usize + 18 + (mlen as usize / self.chunk_size + 1) * 8);

        put_basic_header(out, fmt, m.csid);
        if fmt <= 2 {
            put_be24(out, if ext { EXT_TS } else { tsfield });
            if fmt <= 1 {
                put_be24(out, mlen);
                out.put_u8(m.type_id);
                if fmt == 0 {
                    out.put_u32_le(m.msid);
                }
            }
        }
        // fmt 3 inherits the extended-timestamp flag from the previous header
        // on this csid; fmt 3 is only chosen when that flag matches `ext`.
        if ext {
            out.put_u32(tsfield);
        }

        let mut rest = &m.payload[..];
        let first = rest.len().min(self.chunk_size);
        out.put_slice(&rest[..first]);
        rest = &rest[first..];
        while !rest.is_empty() {
            put_basic_header(out, 3, m.csid);
            if ext {
                out.put_u32(tsfield);
            }
            let n = rest.len().min(self.chunk_size);
            out.put_slice(&rest[..n]);
            rest = &rest[n..];
        }
    }
}

fn put_basic_header(out: &mut BytesMut, fmt: u8, csid: u32) {
    let f = fmt << 6;
    if csid < 64 {
        out.put_u8(f | csid as u8);
    } else if csid < 320 {
        out.put_u8(f);
        out.put_u8((csid - 64) as u8);
    } else {
        let v = csid - 64;
        out.put_u8(f | 1);
        out.put_u8(v as u8);
        out.put_u8((v >> 8) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(csid: u32, ts: u32, t: u8, len: usize) -> Message {
        Message {
            csid,
            timestamp: ts,
            type_id: t,
            msid: 1,
            payload: Bytes::from((0..len).map(|i| i as u8).collect::<Vec<_>>()),
        }
    }

    fn roundtrip(msgs: &[Message], enc_cs: usize, feed: usize) {
        let mut enc = ChunkEncoder::default();
        enc.set_chunk_size(enc_cs);
        let mut wire = BytesMut::new();
        for m in msgs {
            enc.encode(m, &mut wire);
        }
        let mut dec = ChunkDecoder::new(1 << 24, 64, true);
        dec.set_chunk_size(enc_cs);
        let mut input = BytesMut::new();
        let mut got = Vec::new();
        for piece in wire.chunks(feed) {
            input.extend_from_slice(piece);
            while let Some(m) = dec.decode(&mut input).unwrap() {
                got.push(m);
            }
        }
        assert!(input.is_empty());
        assert_eq!(got, msgs);
    }

    #[test]
    fn roundtrip_various() {
        let msgs = vec![
            msg(6, 0, 8, 10),
            msg(7, 0, 9, 1000),
            msg(6, 23, 8, 10),
            msg(6, 46, 8, 10),
            msg(6, 69, 8, 10),
            msg(7, 33, 9, 500),
            msg(7, 66, 9, 0),
            msg(300, 70, 18, 5),
            msg(70, 80, 20, 300),
            msg(7, 0x0100_0000, 9, 400), // extended timestamp, absolute
            msg(7, 0x0100_0010, 9, 400),
        ];
        for cs in [1, 128, 4096] {
            for feed in [1, 7, 4096] {
                roundtrip(&msgs, cs, feed);
            }
        }
    }

    #[test]
    fn big_extended_delta() {
        roundtrip(&[msg(4, 0, 8, 300), msg(4, 0x0200_0000, 8, 300)], 128, 3);
    }

    #[test]
    fn extended_absolute_then_zero_delta() {
        roundtrip(&[msg(5, 0x0100_0000, 9, 10), msg(5, 0x0100_0000, 9, 10)], 128, 1);
    }

    #[test]
    fn too_big_rejected() {
        let mut enc = ChunkEncoder::default();
        let mut wire = BytesMut::new();
        enc.encode(&msg(3, 0, 20, 2000), &mut wire);
        let mut dec = ChunkDecoder::new(1000, 8, true);
        assert!(matches!(dec.decode(&mut wire), Err(ChunkError::TooBig(2000, 1000))));
    }
}
