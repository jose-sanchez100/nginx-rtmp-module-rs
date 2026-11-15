//! Statistics page.
//!
//! Port of `ngx_rtmp_stat_module.c`: an `rtmp_stat` location answers with
//! the XML document nginx-rtmp produces (same elements, same order), so
//! `stat.xsl` and existing monitoring tools keep working.

use crate::codec::CodecInfo;
use crate::live::{Bandwidth, ClientStat, Registry, StreamStat};
use crate::session::ServerCtx;
use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub const GLOBAL: u8 = 0x01;
pub const LIVE: u8 = 0x02;
pub const CLIENTS: u8 = 0x04;
pub const ALL: u8 = 0xff;

/// `rtmp_stat` values.
pub fn parse_flag(v: &str) -> Option<u8> {
    Some(match v {
        "all" => ALL,
        "global" => GLOBAL,
        "live" => LIVE,
        "clients" => CLIENTS,
        _ => return None,
    })
}

/// Text content: markup characters escaped, control characters turned into
/// spaces (`ngx_rtmp_stat_escape`). nginx also blanks every non-ASCII byte;
/// UTF-8 is kept here, the document says it is UTF-8.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

/// `ngx_rtmp_stat_bw`: rate in bits per second and/or the byte total.
fn bw(o: &mut String, b: &Bandwidth, name: &str, rate: bool, bytes: bool) {
    let mut b = b.clone();
    if rate {
        let _ = write!(o, "<bw_{name}>{}</bw_{name}>\r\n", b.rate() * 8);
    }
    if bytes {
        let _ = write!(o, "<bytes_{name}>{}</bytes_{name}>\r\n", b.bytes);
    }
}

fn avc_profile(p: u32) -> &'static str {
    match p {
        66 => "Baseline",
        77 => "Main",
        100 => "High",
        _ => "",
    }
}

fn aac_profile(c: &CodecInfo) -> &'static str {
    match c.aac_profile {
        1 => "Main",
        2 if c.aac_ps => "HEv2",
        2 if c.aac_sbr => "HE",
        2 => "LC",
        3 => "SSR",
        4 => "LTP",
        5 => "SBR",
        _ => "",
    }
}

/// When the running executable was built (its modification time), in the
/// `__DATE__ __TIME__` format nginx reports.
fn built() -> String {
    std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%b %e %Y %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// `ngx_rtmp_stat_client`.
fn client(o: &mut String, c: &ClientStat) {
    let info = &c.peer.info;
    let _ = write!(
        o,
        "<id>{}</id><address>{}</address><time>{}</time>",
        c.peer.id,
        esc(&info.addr),
        info.started.elapsed().as_millis()
    );
    if let Some(ci) = info.connect.get() {
        for (tag, v) in [("flashver", &ci.flashver), ("pageurl", &ci.page_url), ("swfurl", &ci.swf_url)] {
            if !v.is_empty() {
                let _ = write!(o, "<{tag}>{}</{tag}>", esc(v));
            }
        }
    }
}

fn meta(o: &mut String, c: &CodecInfo) {
    let _ = write!(
        o,
        "<meta><video><width>{}</width><height>{}</height><frame_rate>{}</frame_rate>",
        c.width, c.height, c.frame_rate as u64
    );
    let name = c.video_codec_name();
    if !name.is_empty() {
        let _ = write!(o, "<codec>{}</codec>", esc(&name));
    }
    if c.avc_profile != 0 {
        let _ = write!(o, "<profile>{}</profile>", avc_profile(c.avc_profile));
    }
    if c.avc_level != 0 {
        let _ = write!(o, "<compat>{}</compat>", c.avc_compat);
        let _ = write!(o, "<level>{:.1}</level>", c.avc_level as f64 / 10.0);
    }
    o.push_str("</video><audio>");
    let name = c.audio_codec_name();
    if !name.is_empty() {
        let _ = write!(o, "<codec>{name}</codec>");
    }
    if c.aac_profile != 0 {
        let _ = write!(o, "<profile>{}</profile>", aac_profile(c));
    }
    let channels = if c.aac_chan_conf != 0 { c.aac_chan_conf } else { c.audio_channels };
    if channels != 0 {
        let _ = write!(o, "<channels>{channels}</channels>");
    }
    if c.sample_rate != 0 {
        let _ = write!(o, "<sample_rate>{}</sample_rate>", c.sample_rate);
    }
    o.push_str("</audio></meta>\r\n");
}

/// `ngx_rtmp_stat_live`.
fn live(o: &mut String, streams: &[StreamStat], mask: u8) {
    o.push_str("<live>\r\n");
    let mut total = 0;
    for s in streams {
        let _ = write!(o, "<stream>\r\n<name>{}</name>\r\n<time>{}</time>", esc(&s.name), s.time_ms);
        bw(o, &s.bw_in, "in", true, true);
        bw(o, &s.bw_out, "out", true, true);
        bw(o, &s.bw_audio, "audio", true, false);
        bw(o, &s.bw_video, "video", true, false);
        if mask & CLIENTS != 0 {
            for c in &s.clients {
                o.push_str("<client>");
                client(o, c);
                let _ = write!(
                    o,
                    "<dropped>{}</dropped><avsync>{}</avsync><timestamp>{}</timestamp>",
                    c.dropped,
                    c.avsync,
                    c.peer.info.last_ts.load(Ordering::Relaxed) as i32
                );
                if c.publishing {
                    o.push_str("<publishing/>");
                }
                if s.active {
                    o.push_str("<active/>");
                }
                o.push_str("</client>\r\n");
            }
        }
        total += s.clients.len();
        if let Some(c) = &s.codec {
            meta(o, c);
        }
        let _ = write!(o, "<nclients>{}</nclients>\r\n", s.clients.len());
        if s.publishing {
            o.push_str("<publishing/>\r\n");
        }
        if s.active {
            o.push_str("<active/>\r\n");
        }
        o.push_str("</stream>\r\n");
    }
    let _ = write!(o, "<nclients>{total}</nclients>\r\n</live>\r\n");
}

/// `ngx_rtmp_stat_handler`: the whole document.
pub fn xml(servers: &[Arc<ServerCtx>], registry: &Registry, mask: u8, stylesheet: &str) -> String {
    let mut o = String::with_capacity(4096);
    o.push_str("<?xml version=\"1.0\" encoding=\"utf-8\" ?>\r\n");
    if !stylesheet.is_empty() {
        let _ = write!(o, "<?xml-stylesheet type=\"text/xsl\" href=\"{}\" ?>\r\n", esc(stylesheet));
    }
    let g = &registry.stats;
    let _ = write!(
        o,
        "<rtmp>\r\n<nginx_version>nginx-rtmp-module-rs/{}</nginx_version>\r\n\
         <nginx_rtmp_version>1.1.4</nginx_rtmp_version>\r\n<compiler>rustc</compiler>\r\n\
         <built>{}</built>\r\n<pid>{}</pid>\r\n<uptime>{}</uptime>\r\n<naccepted>{}</naccepted>\r\n",
        env!("CARGO_PKG_VERSION"),
        built(),
        std::process::id(),
        g.started.elapsed().as_secs(),
        g.naccepted.load(Ordering::Relaxed)
    );
    bw(&mut o, &g.bw_in.lock().unwrap(), "in", true, true);
    bw(&mut o, &g.bw_out.lock().unwrap(), "out", true, true);
    for ctx in servers {
        o.push_str("<server>\r\n");
        for app in &ctx.apps {
            let _ = write!(o, "<application>\r\n<name>{}</name>\r\n", esc(&app.name));
            if mask & LIVE != 0 && app.live {
                live(&mut o, &registry.stat(ctx.index, &app.name), mask);
            }
            o.push_str("</application>\r\n");
        }
        o.push_str("</server>\r\n");
    }
    o.push_str("</rtmp>\r\n");
    o
}
