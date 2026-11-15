//! RTMP message types and builders for protocol-control / command messages.
//!
//! Port of the constants in `ngx_rtmp.h` and the builders in `ngx_rtmp_send.c`.

use crate::amf::{self, Amf0};
use crate::chunk::Message;
use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const MSG_CHUNK_SIZE: u8 = 1;
pub const MSG_ABORT: u8 = 2;
pub const MSG_ACK: u8 = 3;
pub const MSG_USER: u8 = 4;
pub const MSG_ACK_SIZE: u8 = 5;
pub const MSG_BANDWIDTH: u8 = 6;
pub const MSG_EDGE: u8 = 7;
pub const MSG_AUDIO: u8 = 8;
pub const MSG_VIDEO: u8 = 9;
pub const MSG_AMF3_META: u8 = 15;
pub const MSG_AMF3_SHARED: u8 = 16;
pub const MSG_AMF3_CMD: u8 = 17;
pub const MSG_AMF_META: u8 = 18;
pub const MSG_AMF_SHARED: u8 = 19;
pub const MSG_AMF_CMD: u8 = 20;
pub const MSG_AGGREGATE: u8 = 22;

pub const USER_STREAM_BEGIN: u16 = 0;
pub const USER_STREAM_EOF: u16 = 1;
pub const USER_STREAM_DRY: u16 = 2;
pub const USER_SET_BUFLEN: u16 = 3;
pub const USER_RECORDED: u16 = 4;
pub const USER_PING_REQUEST: u16 = 6;
pub const USER_PING_RESPONSE: u16 = 7;

pub const LIMIT_HARD: u8 = 0;
pub const LIMIT_SOFT: u8 = 1;
pub const LIMIT_DYNAMIC: u8 = 2;

/// Chunk stream ids used by nginx-rtmp for outgoing traffic.
pub const CSID_CTL: u32 = 2;
pub const CSID_AMF_INI: u32 = 3;
pub const CSID_AMF: u32 = 5;
pub const CSID_AUDIO: u32 = 6;
pub const CSID_VIDEO: u32 = 7;

/// nginx-rtmp always hands out message stream id 1.
pub const MSID: u32 = 1;

pub fn type_name(t: u8) -> &'static str {
    match t {
        MSG_CHUNK_SIZE => "chunk_size",
        MSG_ABORT => "abort",
        MSG_ACK => "ack",
        MSG_USER => "user",
        MSG_ACK_SIZE => "ack_size",
        MSG_BANDWIDTH => "bandwidth",
        MSG_EDGE => "edge",
        MSG_AUDIO => "audio",
        MSG_VIDEO => "video",
        MSG_AMF3_META => "amf3_meta",
        MSG_AMF3_SHARED => "amf3_shared",
        MSG_AMF3_CMD => "amf3_cmd",
        MSG_AMF_META => "amf_meta",
        MSG_AMF_SHARED => "amf_shared",
        MSG_AMF_CMD => "amf_cmd",
        MSG_AGGREGATE => "aggregate",
        _ => "?",
    }
}

fn ctl(type_id: u8, payload: Bytes) -> Message {
    Message { csid: CSID_CTL, timestamp: 0, type_id, msid: 0, payload }
}

fn u32_payload(v: u32) -> Bytes {
    Bytes::copy_from_slice(&v.to_be_bytes())
}

pub fn chunk_size(size: u32) -> Message {
    ctl(MSG_CHUNK_SIZE, u32_payload(size))
}

pub fn abort(csid: u32) -> Message {
    ctl(MSG_ABORT, u32_payload(csid))
}

pub fn ack(seq: u32) -> Message {
    ctl(MSG_ACK, u32_payload(seq))
}

pub fn ack_size(size: u32) -> Message {
    ctl(MSG_ACK_SIZE, u32_payload(size))
}

pub fn bandwidth(size: u32, limit: u8) -> Message {
    let mut b = BytesMut::with_capacity(5);
    b.put_u32(size);
    b.put_u8(limit);
    ctl(MSG_BANDWIDTH, b.freeze())
}

fn user(event: u16, args: &[u32]) -> Message {
    let mut b = BytesMut::with_capacity(2 + 4 * args.len());
    b.put_u16(event);
    for a in args {
        b.put_u32(*a);
    }
    ctl(MSG_USER, b.freeze())
}

pub fn stream_begin(msid: u32) -> Message {
    user(USER_STREAM_BEGIN, &[msid])
}
pub fn stream_eof(msid: u32) -> Message {
    user(USER_STREAM_EOF, &[msid])
}
pub fn stream_dry(msid: u32) -> Message {
    user(USER_STREAM_DRY, &[msid])
}
pub fn set_buflen(msid: u32, buflen_ms: u32) -> Message {
    user(USER_SET_BUFLEN, &[msid, buflen_ms])
}
pub fn recorded(msid: u32) -> Message {
    user(USER_RECORDED, &[msid])
}
pub fn ping_request(timestamp: u32) -> Message {
    user(USER_PING_REQUEST, &[timestamp])
}
pub fn ping_response(timestamp: u32) -> Message {
    user(USER_PING_RESPONSE, &[timestamp])
}

/// AMF0 command message (type 20).
pub fn amf_cmd(csid: u32, msid: u32, values: &[Amf0]) -> Message {
    Message { csid, timestamp: 0, type_id: MSG_AMF_CMD, msid, payload: amf::encode_all(values).freeze() }
}

/// AMF0 data message (type 18), e.g. `onMetaData` or `|RtmpSampleAccess`.
pub fn amf_data(csid: u32, msid: u32, values: &[Amf0]) -> Message {
    Message { csid, timestamp: 0, type_id: MSG_AMF_META, msid, payload: amf::encode_all(values).freeze() }
}

/// `onStatus` notification (`ngx_rtmp_create_status`).
pub fn status(code: &str, level: &str, desc: &str) -> Message {
    amf_cmd(
        CSID_AMF,
        MSID,
        &[
            Amf0::str("onStatus"),
            Amf0::Number(0.0),
            Amf0::Null,
            Amf0::object([
                ("level", Amf0::str(level)),
                ("code", Amf0::str(code)),
                ("description", Amf0::str(desc)),
            ]),
        ],
    )
}

/// `|RtmpSampleAccess` (`ngx_rtmp_create_sample_access`).
pub fn sample_access() -> Message {
    amf_data(
        CSID_AMF,
        MSID,
        &[Amf0::str("|RtmpSampleAccess"), Amf0::Boolean(true), Amf0::Boolean(true)],
    )
}

/// Split an aggregate message (type 22) into its sub-messages
/// (`ngx_rtmp_aggregate_message_handler`). Sub-message timestamps are made
/// relative to the first one and rebased on the aggregate's timestamp.
pub fn split_aggregate(m: &Message) -> Vec<Message> {
    let mut out = Vec::new();
    let mut p = m.payload.clone();
    let mut base: Option<u32> = None;
    while p.len() >= 11 {
        let t = p[0];
        let size = (p[1] as usize) << 16 | (p[2] as usize) << 8 | p[3] as usize;
        let ts = (p[4] as u32) << 16 | (p[5] as u32) << 8 | p[6] as u32 | (p[7] as u32) << 24;
        if p.len() < 11 + size {
            break;
        }
        let body = p.slice(11..11 + size);
        p.advance((11 + size + 4).min(p.len()));
        let first = *base.get_or_insert(ts);
        out.push(Message {
            csid: m.csid,
            timestamp: m.timestamp.wrapping_add(ts.wrapping_sub(first)),
            type_id: t,
            msid: m.msid,
            payload: body,
        });
    }
    out
}
