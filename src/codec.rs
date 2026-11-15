//! FLV audio/video tag inspection and metadata handling.
//!
//! Port of the parts of `ngx_rtmp_codec_module.c` the live module depends on:
//! codec/sequence-header detection, key-frame detection and the `meta`
//! directive. Enhanced RTMP (HEVC/AV1/VP9 via FourCC "ex" headers, as sent
//! by recent OBS versions) is recognised too, which the C module predates.

use crate::amf::{self, Amf0};
use crate::config::MetaMode;
use bytes::Bytes;

pub const AUDIO_AAC: u8 = 10;
pub const VIDEO_H264: u8 = 7;
pub const VIDEO_HEVC_LEGACY: u8 = 12;

pub const FRAME_KEY: u8 = 1;
pub const FRAME_INTER: u8 = 2;
pub const FRAME_DISPOSABLE: u8 = 3;

pub const SERVER_NAME: &str = "NGINX RTMP (github.com/arut/nginx-rtmp-module)";

fn is_ex_video(p: &[u8]) -> bool {
    !p.is_empty() && p[0] & 0x80 != 0
}

/// Video frame type (1 = key, 2 = inter, 3 = disposable, 5 = info/command).
pub fn video_frame_type(p: &[u8]) -> u8 {
    match p.first() {
        Some(&b) if is_ex_video(p) => (b >> 4) & 0x07,
        Some(&b) => (b >> 4) & 0x0f,
        None => 0,
    }
}

pub fn audio_codec_id(p: &[u8]) -> u8 {
    p.first().map(|b| b >> 4).unwrap_or(0)
}

pub fn video_codec_id(p: &[u8]) -> u8 {
    p.first().map(|b| b & 0x0f).unwrap_or(0)
}

/// AAC sequence header (AudioSpecificConfig).
pub fn is_audio_header(p: &[u8]) -> bool {
    p.len() > 1 && audio_codec_id(p) == AUDIO_AAC && p[1] == 0
}

/// AVC/HEVC decoder configuration record, legacy or enhanced-RTMP.
pub fn is_video_header(p: &[u8]) -> bool {
    if is_ex_video(p) {
        // PacketType 0 = SequenceStart
        return p[0] & 0x0f == 0;
    }
    p.len() > 1 && matches!(video_codec_id(p), VIDEO_H264 | VIDEO_HEVC_LEGACY) && p[1] == 0
}

/// Codec parameters announced in `onMetaData` (subset kept by nginx).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MetaInfo {
    pub width: f64,
    pub height: f64,
    pub duration: f64,
    pub frame_rate: f64,
    pub video_data_rate: f64,
    pub video_codec_id: f64,
    pub audio_data_rate: f64,
    pub audio_codec_id: f64,
    pub profile: String,
    pub level: String,
}

fn codec_id(v: Option<&Amf0>) -> f64 {
    match v {
        Some(Amf0::Number(n)) => *n,
        // FourCC strings sent by some encoders
        Some(Amf0::String(s)) => match s.as_str() {
            "avc1" | "h264" => VIDEO_H264 as f64,
            "mp4a" | "aac" => AUDIO_AAC as f64,
            "mp3" | ".mp3" => 2.0,
            "hvc1" | "hev1" => VIDEO_HEVC_LEGACY as f64,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

impl MetaInfo {
    fn from_props(o: &Amf0) -> Self {
        let num = |k: &str| o.get(k).and_then(Amf0::as_f64).unwrap_or(0.0);
        let s = |k: &str| o.get(k).and_then(Amf0::as_str).unwrap_or("").to_string();
        let mut frame_rate = num("framerate");
        if frame_rate == 0.0 {
            frame_rate = num("fps");
        }
        MetaInfo {
            width: num("width"),
            height: num("height"),
            duration: num("duration"),
            frame_rate,
            video_data_rate: num("videodatarate"),
            video_codec_id: codec_id(o.get("videocodecid")),
            audio_data_rate: num("audiodatarate"),
            audio_codec_id: codec_id(o.get("audiocodecid")),
            profile: s("profile"),
            level: s("level"),
        }
    }
}

/// Process an incoming AMF data message from a publisher.
///
/// Returns `Some((payload, info))` when it is stream metadata: the payload
/// is the `onMetaData` message to forward to players (shape depends on the
/// `meta` directive) and `info` the parsed codec parameters.
pub fn process_metadata(values: &[Amf0], mode: MetaMode) -> Option<(Option<Bytes>, MetaInfo)> {
    // Strip "@setDataFrame" wrapper used by publishers.
    let values = match values.first().and_then(Amf0::as_str) {
        Some("@setDataFrame") => &values[1..],
        _ => values,
    };
    if values.first().and_then(Amf0::as_str) != Some("onMetaData") {
        return None;
    }
    let obj = values.get(1)?;
    let info = MetaInfo::from_props(obj);
    let payload = match mode {
        MetaMode::Off => None,
        MetaMode::Copy => Some(amf::encode_all(values).freeze()),
        MetaMode::On => {
            let n = Amf0::Number;
            let props = vec![
                ("Server", Amf0::str(SERVER_NAME)),
                ("width", n(info.width)),
                ("height", n(info.height)),
                ("displayWidth", n(info.width)),
                ("displayHeight", n(info.height)),
                ("duration", n(info.duration)),
                ("framerate", n(info.frame_rate)),
                ("fps", n(info.frame_rate)),
                ("videodatarate", n(info.video_data_rate)),
                ("videocodecid", n(info.video_codec_id)),
                ("audiodatarate", n(info.audio_data_rate)),
                ("audiocodecid", n(info.audio_codec_id)),
                ("profile", Amf0::str(info.profile.clone())),
                ("level", Amf0::str(info.level.clone())),
            ];
            Some(amf::encode_all(&[Amf0::str("onMetaData"), Amf0::object(props)]).freeze())
        }
    };
    Some((payload, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_headers() {
        assert!(is_video_header(&[0x17, 0x00, 0, 0, 0]));
        assert!(!is_video_header(&[0x17, 0x01, 0, 0, 0]));
        assert_eq!(video_frame_type(&[0x17, 0x01]), FRAME_KEY);
        assert_eq!(video_frame_type(&[0x27, 0x01]), FRAME_INTER);
        assert!(is_audio_header(&[0xaf, 0x00, 0x12, 0x10]));
        assert!(!is_audio_header(&[0xaf, 0x01]));
        // enhanced RTMP: IsExHeader | key frame | SequenceStart, "hvc1"
        assert!(is_video_header(&[0x90, b'h', b'v', b'c', b'1']));
        assert_eq!(video_frame_type(&[0x91, b'h', b'v', b'c', b'1']), FRAME_KEY);
    }

    #[test]
    fn metadata_modes() {
        let vals = vec![
            Amf0::str("@setDataFrame"),
            Amf0::str("onMetaData"),
            Amf0::EcmaArray(vec![
                ("width".into(), Amf0::Number(1280.0)),
                ("videocodecid".into(), Amf0::str("avc1")),
                ("encoder".into(), Amf0::str("obs")),
            ]),
        ];
        let (copy, info) = process_metadata(&vals, MetaMode::Copy).unwrap();
        assert_eq!(info.width, 1280.0);
        assert_eq!(info.video_codec_id, 7.0);
        let copy = amf::decode_all(&copy.unwrap()).unwrap();
        assert_eq!(copy[0], Amf0::str("onMetaData"));
        assert!(copy[1].get("encoder").is_some());

        let (on, _) = process_metadata(&vals, MetaMode::On).unwrap();
        let on = amf::decode_all(&on.unwrap()).unwrap();
        assert_eq!(on[1].get("Server").and_then(Amf0::as_str), Some(SERVER_NAME));
        assert!(on[1].get("encoder").is_none());

        assert!(process_metadata(&vals, MetaMode::Off).unwrap().0.is_none());
        assert!(process_metadata(&[Amf0::str("onCuePoint")], MetaMode::On).is_none());
    }
}
