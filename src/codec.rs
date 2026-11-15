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

/// Bit reader over a byte slice (`ngx_rtmp_bit_reader_t`): reading past
/// the end yields zeros.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read(&mut self, n: u32) -> u64 {
        let mut v = 0u64;
        for _ in 0..n {
            let bit = self.data.get(self.pos / 8).map_or(0, |b| (b >> (7 - self.pos % 8)) & 1);
            v = (v << 1) | bit as u64;
            self.pos += 1;
        }
        v
    }

    fn eof(&self) -> bool {
        self.pos >= self.data.len() * 8
    }

    /// Unsigned Exp-Golomb (`ngx_rtmp_bit_read_golomb`).
    fn ue(&mut self) -> u64 {
        let mut n = 0;
        while self.read(1) == 0 {
            if self.eof() || n >= 32 {
                return 0;
            }
            n += 1;
        }
        ((1u64 << n) - 1) + self.read(n)
    }

    fn se(&mut self) -> i64 {
        let v = self.ue() as i64;
        if v & 1 == 1 { (v + 1) / 2 } else { -(v / 2) }
    }
}

/// Fields of an AVC decoder configuration record and its first SPS
/// (`ngx_rtmp_codec_parse_avc_header`).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct AvcInfo {
    pub profile: u32,
    pub compat: u32,
    pub level: u32,
    pub nal_bytes: u32,
    pub ref_frames: u32,
    /// 0 when the record carries no SPS.
    pub width: u32,
    pub height: u32,
}

/// Parse an FLV AVC sequence header (the whole video tag body).
pub fn parse_avc_header(p: &[u8]) -> AvcInfo {
    let mut br = BitReader::new(p);
    // codec/frame type, packet type, composition time, configurationVersion
    br.read(48);
    let mut info = AvcInfo {
        profile: br.read(8) as u32,
        compat: br.read(8) as u32,
        level: br.read(8) as u32,
        nal_bytes: (br.read(8) as u32 & 0x03) + 1,
        ..Default::default()
    };
    if br.read(8) & 0x1f == 0 {
        return info; // no SPS
    }
    br.read(16); // SPS length
    // nginx insists on 0x67; any nal_ref_idc is accepted here.
    if br.read(8) & 0x1f != 7 {
        return info;
    }
    let profile_idc = br.read(8);
    br.read(16); // constraint flags, level_idc
    br.ue(); // seq_parameter_set_id
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma_format_idc = br.ue();
        if chroma_format_idc == 3 {
            br.read(1); // separate_colour_plane_flag
        }
        br.ue(); // bit_depth_luma_minus8
        br.ue(); // bit_depth_chroma_minus8
        br.read(1); // qpprime_y_zero_transform_bypass_flag
        if br.read(1) == 1 {
            // seq_scaling_matrix_present_flag; nginx does not skip the
            // lists themselves, which shifts everything after them.
            for i in 0..if chroma_format_idc != 3 { 8 } else { 12 } {
                if br.read(1) == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + br.se() + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    br.ue(); // log2_max_frame_num_minus4
    match br.ue() {
        0 => {
            br.ue(); // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            br.read(1); // delta_pic_order_always_zero_flag
            br.se(); // offset_for_non_ref_pic
            br.se(); // offset_for_top_to_bottom_field
            let n = br.ue().min(255);
            for _ in 0..n {
                br.se(); // offset_for_ref_frame
            }
        }
        _ => {}
    }
    info.ref_frames = br.ue() as u32;
    br.read(1); // gaps_in_frame_num_value_allowed_flag
    let width = br.ue();
    let height = br.ue();
    let frame_mbs_only = br.read(1);
    if frame_mbs_only == 0 {
        br.read(1); // mb_adaptive_frame_field_flag
    }
    br.read(1); // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if br.read(1) == 1 {
        cl = br.ue();
        cr = br.ue();
        ct = br.ue();
        cb = br.ue();
    }
    let w = (width + 1) * 16;
    let h = (2 - frame_mbs_only) * (height + 1) * 16;
    info.width = w.saturating_sub((cl + cr) * 2) as u32;
    info.height = h.saturating_sub((ct + cb) * 2) as u32;
    info
}

/// Fields of an AAC AudioSpecificConfig (`ngx_rtmp_codec_parse_aac_header`).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct AacInfo {
    /// Audio object type (2 = LC, ...); for SBR/PS streams, the core type.
    pub profile: u32,
    pub sample_rate: u32,
    pub chan_conf: u32,
    pub sbr: bool,
    pub ps: bool,
}

const AAC_SAMPLE_RATES: [u32; 16] =
    [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 0, 0, 0];

/// Parse an FLV AAC sequence header (the whole audio tag body).
pub fn parse_aac_header(p: &[u8]) -> AacInfo {
    let mut br = BitReader::new(p);
    br.read(16); // sound format byte, AAC packet type
    let object_type = |br: &mut BitReader| {
        let t = br.read(5) as u32;
        if t == 31 { br.read(6) as u32 + 32 } else { t }
    };
    let rate = |br: &mut BitReader| {
        let idx = br.read(4) as usize;
        if idx == 15 { br.read(24) as u32 } else { AAC_SAMPLE_RATES[idx] }
    };
    let mut info = AacInfo { profile: object_type(&mut br), ..Default::default() };
    info.sample_rate = rate(&mut br);
    info.chan_conf = br.read(4) as u32;
    if info.profile == 5 || info.profile == 29 {
        info.ps = info.profile == 29;
        info.sbr = true;
        info.sample_rate = rate(&mut br);
        info.profile = object_type(&mut br);
    }
    info
}

pub const AUDIO_CODECS: [&str; 17] = [
    "", "ADPCM", "MP3", "LinearLE", "Nellymoser16", "Nellymoser8", "Nellymoser", "G711A", "G711U", "", "AAC",
    "Speex", "", "", "MP3-8K", "DeviceSpecific", "Uncompressed",
];

pub const VIDEO_CODECS: [&str; 8] =
    ["", "Jpeg", "Sorenson-H263", "ScreenVideo", "On2-VP6", "On2-VP6-Alpha", "ScreenVideo2", "H264"];

/// What the codec module knows about a published stream
/// (`ngx_rtmp_codec_ctx_t`), for the statistics page.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CodecInfo {
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
    pub video_codec_id: u32,
    /// Enhanced RTMP FourCC ("hvc1", "av01", ...), when the stream uses it.
    pub video_fourcc: Option<[u8; 4]>,
    pub avc_profile: u32,
    pub avc_compat: u32,
    pub avc_level: u32,
    pub audio_codec_id: u32,
    pub audio_channels: u32,
    pub sample_rate: u32,
    pub aac_profile: u32,
    pub aac_chan_conf: u32,
    pub aac_sbr: bool,
    pub aac_ps: bool,
}

impl CodecInfo {
    /// `ngx_rtmp_codec_av`: codec ids from every frame, details from the
    /// sequence headers.
    pub fn on_av(&mut self, video: bool, p: &[u8]) {
        let Some(&fmt) = p.first() else { return };
        if !video {
            self.audio_codec_id = (fmt >> 4) as u32;
            self.audio_channels = (fmt & 0x01) as u32 + 1;
            if self.sample_rate == 0 {
                self.sample_rate = [5512, 11025, 22050, 44100][((fmt & 0x0c) >> 2) as usize];
            }
            if is_audio_header(p) && p.len() >= 3 {
                let a = parse_aac_header(p);
                self.aac_profile = a.profile;
                self.sample_rate = a.sample_rate;
                self.aac_chan_conf = a.chan_conf;
                self.aac_sbr = a.sbr;
                self.aac_ps = a.ps;
            }
            return;
        }
        if is_ex_video(p) {
            if let Some(f) = p.get(1..5) {
                self.video_fourcc = Some(f.try_into().unwrap());
            }
            return;
        }
        self.video_fourcc = None;
        self.video_codec_id = (fmt & 0x0f) as u32;
        if self.video_codec_id == VIDEO_H264 as u32 && is_video_header(p) && p.len() >= 3 {
            let a = parse_avc_header(p);
            self.avc_profile = a.profile;
            self.avc_compat = a.compat;
            self.avc_level = a.level;
            if a.width > 0 {
                self.width = a.width;
                self.height = a.height;
            }
        }
    }

    /// `ngx_rtmp_codec_meta_data`: values announced in `onMetaData`.
    pub fn on_meta(&mut self, m: &MetaInfo) {
        self.width = m.width as u32;
        self.height = m.height as u32;
        self.frame_rate = m.frame_rate;
        self.video_codec_id = m.video_codec_id as u32;
        self.audio_codec_id = m.audio_codec_id as u32;
    }

    pub fn video_codec_name(&self) -> String {
        match self.video_fourcc {
            Some(f) => match &f {
                b"hvc1" | b"hev1" => "HEVC".into(),
                b"av01" => "AV1".into(),
                b"vp09" => "VP9".into(),
                b"avc1" => "H264".into(),
                _ => String::from_utf8_lossy(&f).into_owned(),
            },
            None if self.video_codec_id == VIDEO_HEVC_LEGACY as u32 => "HEVC".into(),
            None => VIDEO_CODECS.get(self.video_codec_id as usize).copied().unwrap_or("").into(),
        }
    }

    pub fn audio_codec_name(&self) -> &'static str {
        AUDIO_CODECS.get(self.audio_codec_id as usize).copied().unwrap_or("")
    }
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
    fn parse_headers() {
        // x264 640x360 High@3.0 avcC, as sent by ffmpeg.
        let avc = [
            0x17, 0x00, 0, 0, 0, 0x01, 0x64, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x19, 0x67, 0x64, 0x00, 0x1e, 0xac,
            0xd9, 0x40, 0xa0, 0x2f, 0xf9, 0x70, 0x11, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03, 0x00,
            0x32, 0x0f, 0x16, 0x2d, 0x96, 0x01, 0x00, 0x06, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0,
        ];
        let a = parse_avc_header(&avc);
        assert_eq!((a.profile, a.compat, a.level, a.nal_bytes), (100, 0, 30, 4));
        assert_eq!((a.width, a.height), (640, 360));
        // AAC LC, 44100 Hz, stereo
        let aac = parse_aac_header(&[0xaf, 0x00, 0x12, 0x10]);
        assert_eq!((aac.profile, aac.sample_rate, aac.chan_conf, aac.sbr), (2, 44100, 2, false));
        // HE-AAC v2: object type 29, 24 kHz, stereo, 48 kHz extension, then LC
        let he = parse_aac_header(&[0xaf, 0x00, 0xeb, 0x09, 0x88, 0x00]);
        assert!(he.ps && he.sbr);
        assert_eq!(he.profile, 2);

        let mut c = CodecInfo::default();
        c.on_av(false, &[0xaf, 0x00, 0x12, 0x10]);
        c.on_av(true, &avc);
        assert_eq!((c.audio_codec_name(), c.video_codec_name().as_str()), ("AAC", "H264"));
        assert_eq!((c.sample_rate, c.audio_channels, c.width), (44100, 2, 640));
        c.on_av(true, &[0x90, b'h', b'v', b'c', b'1']);
        assert_eq!(c.video_codec_name(), "HEVC");
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
