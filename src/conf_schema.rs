//! What the configuration editor offers: every directive this server
//! understands, where it may be written, its arguments, default and a
//! short description.
//!
//! Contexts: `main` (top level), `rtmp`, `server` (inside `rtmp`),
//! `application`, `recorder`, `http`, `http_server` (a `server` inside
//! `http`) and `location`. Server-level settings written in `rtmp {}` are
//! defaults for every `server {}`; application-level settings written in
//! `rtmp {}` or `server {}` are defaults for every `application {}`.

use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "type", content = "values", rename_all = "lowercase")]
pub enum Kind {
    /// `on` / `off`.
    Flag,
    /// nginx time: `500ms`, `30s`, `1m`, `1h`.
    Time,
    /// nginx size: `4096`, `8k`, `1m`.
    Size,
    Int,
    Text,
    Path,
    Url,
    /// `port`, `ip:port` or `[ipv6]:port`.
    Addr,
    /// One of these values.
    Choice(&'static [&'static str]),
    /// Any of these values, each one a separate argument.
    Choices(&'static [&'static str]),
    /// `key=value` parameters with these keys.
    Param(&'static [&'static str]),
}

#[derive(Serialize)]
pub struct Arg {
    pub name: &'static str,
    pub kind: Kind,
    /// May be left out.
    pub opt: bool,
    /// Takes any number of values (always the last argument).
    pub rep: bool,
}

#[derive(Serialize)]
pub struct Spec {
    pub name: &'static str,
    pub module: &'static str,
    pub ctx: &'static [&'static str],
    pub args: Vec<Arg>,
    /// Opens a block; the value is the context inside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<&'static str>,
    /// May be written more than once at the same level.
    pub multi: bool,
    pub default: &'static str,
    pub help: &'static str,
}

const MAIN: &[&str] = &["main"];
const RTMP: &[&str] = &["rtmp"];
const SRV: &[&str] = &["rtmp", "server"];
const APP: &[&str] = &["rtmp", "server", "application"];
const REC: &[&str] = &["rtmp", "server", "application", "recorder"];
const HTTP_LOC: &[&str] = &["http", "http_server", "location"];

fn a(name: &'static str, kind: Kind) -> Arg {
    Arg { name, kind, opt: false, rep: false }
}
fn opt(name: &'static str, kind: Kind) -> Arg {
    Arg { name, kind, opt: true, rep: false }
}
fn rep(name: &'static str, kind: Kind) -> Arg {
    Arg { name, kind, opt: true, rep: true }
}

struct B(Spec);

impl B {
    fn new(name: &'static str, module: &'static str, ctx: &'static [&'static str], help: &'static str) -> Self {
        B(Spec { name, module, ctx, args: Vec::new(), block: None, multi: false, default: "", help })
    }
    fn arg(mut self, arg: Arg) -> Self {
        self.0.args.push(arg);
        self
    }
    fn def(mut self, d: &'static str) -> Self {
        self.0.default = d;
        self
    }
    fn multi(mut self) -> Self {
        self.0.multi = true;
        self
    }
    fn block(mut self, inner: &'static str) -> Self {
        self.0.block = Some(inner);
        self
    }
}

fn flag(name: &'static str, module: &'static str, ctx: &'static [&'static str], def: &'static str, help: &'static str) -> Spec {
    B::new(name, module, ctx, help).arg(a("value", Kind::Flag)).def(def).0
}
fn time(name: &'static str, module: &'static str, ctx: &'static [&'static str], def: &'static str, help: &'static str) -> Spec {
    B::new(name, module, ctx, help).arg(a("time", Kind::Time)).def(def).0
}
fn url(name: &'static str, ctx: &'static [&'static str], help: &'static str) -> Spec {
    B::new(name, "notify", ctx, help).arg(a("url", Kind::Url)).0
}
fn exec(name: &'static str, help: &'static str) -> Spec {
    B::new(name, "exec", APP, help).arg(a("command", Kind::Path)).arg(rep("arguments", Kind::Text)).multi().0
}

const RELAY_KEYS: &[&str] = &["app", "name", "playPath", "tcUrl", "pageUrl", "swfUrl", "flashVer", "live", "start", "stop"];
const PULL_KEYS: &[&str] =
    &["app", "name", "playPath", "tcUrl", "pageUrl", "swfUrl", "flashVer", "live", "start", "stop", "static"];

/// All directives, in the order the editor lists them.
pub fn specs() -> Vec<Spec> {
    vec![
        // ---- main ----
        B::new("rtmp", "core", MAIN, "RTMP configuration: server blocks and the defaults they share.").block("rtmp").0,
        B::new("http", "http", MAIN, "HTTP servers. Only servers with rtmp_stat or rtmp_control are started.")
            .block("http")
            .0,
        B::new("error_log", "core", MAIN, "Error log file and the lowest level written to it.")
            .arg(a("file", Kind::Path))
            .arg(opt("level", Kind::Choice(&crate::config::ERROR_LOG_LEVELS)))
            .def("logs/error.log error")
            .0,
        B::new("pid", "core", MAIN, "File holding the process id, used by -s stop / quit / reload.")
            .arg(a("file", Kind::Path))
            .def("logs/nginx.pid")
            .0,
        // ---- rtmp ----
        B::new("server", "core", RTMP, "An RTMP server: its listen addresses and applications.").block("server").multi().0,
        B::new("log_format", "log", RTMP, "Named access log format; $variables: remote_addr, app, name, command, ...")
            .arg(a("name", Kind::Text))
            .arg(rep("format", Kind::Text))
            .multi()
            .0,
        // ---- server ----
        B::new("application", "core", &["server"], "An application: rtmp://host/<name>/<stream>.")
            .arg(a("name", Kind::Text))
            .block("application")
            .multi()
            .0,
        B::new("listen", "core", SRV, "Address to accept RTMP connections on; add ssl for RTMPS.")
            .arg(a("address", Kind::Addr))
            .arg(rep("options", Kind::Choices(&["ssl", "bind", "ipv6only=on", "ipv6only=off", "so_keepalive=on", "so_keepalive=off"])))
            .def("1935")
            .multi()
            .0,
        B::new("ssl_certificate", "core", SRV, "PEM certificate (chain) for listen ... ssl.").arg(a("file", Kind::Path)).0,
        B::new("ssl_certificate_key", "core", SRV, "PEM private key for listen ... ssl.").arg(a("file", Kind::Path)).0,
        time("timeout", "core", SRV, "60s", "Socket timeout: a client that sends nothing for this long is dropped."),
        time("ping", "core", SRV, "60s", "Interval between pings sent to idle clients (0 disables them)."),
        time("ping_timeout", "core", SRV, "30s", "How long to wait for the answer to a ping."),
        B::new("max_streams", "core", SRV, "Maximum number of RTMP streams per connection.").arg(a("number", Kind::Int)).def("32").0,
        B::new("ack_window", "core", SRV, "Acknowledgement window size, in bytes.").arg(a("bytes", Kind::Int)).def("5000000").0,
        B::new("chunk_size", "core", SRV, "Maximum chunk size for outgoing messages (at least 128).")
            .arg(a("size", Kind::Size))
            .def("4096")
            .0,
        B::new("max_message", "core", SRV, "Largest incoming message accepted.").arg(a("size", Kind::Size)).def("1m").0,
        B::new("out_queue", "core", SRV, "Messages queued per client before frames are dropped.")
            .arg(a("number", Kind::Int))
            .def("256")
            .0,
        B::new("out_cork", "core", SRV, "Queued messages that trigger a write.").arg(a("number", Kind::Int)).def("out_queue / 8").0,
        flag("play_time_fix", "core", SRV, "on", "Accepted for compatibility (timestamps are forwarded as they are)."),
        flag("publish_time_fix", "core", SRV, "on", "Accepted for compatibility."),
        flag("busy", "core", SRV, "off", "Accepted for compatibility."),
        time("buflen", "core", SRV, "1s", "Default buffer length announced to players."),
        flag("so_keepalive", "core", SRV, "off", "TCP keepalive on client sockets (accepted, not applied yet)."),
        url("on_connect", SRV, "HTTP callback on connect; a non-2xx answer rejects the client."),
        url("on_disconnect", SRV, "HTTP callback when a client disconnects."),
        time("netcall_timeout", "notify", SRV, "10s", "Time limit for each HTTP callback."),
        B::new("netcall_buffer", "notify", SRV, "Accepted for compatibility.").arg(a("size", Kind::Size)).0,
        // ---- application: live ----
        flag("live", "live", APP, "off", "Live streaming: players get what the publisher sends."),
        flag("wait_key", "live", APP, "on", "Start players on a keyframe."),
        flag("wait_video", "live", APP, "off", "Hold audio until the first video frame."),
        flag("interleave", "live", APP, "off", "Send audio and video on the same chunk stream."),
        time("sync", "live", APP, "300ms", "Resynchronise players that fall this far behind."),
        flag("publish_notify", "live", APP, "off", "Send NetStream.Play.PublishNotify / UnpublishNotify to players."),
        flag("play_restart", "live", APP, "off", "Send NetStream.Play.Start again when the publisher restarts."),
        flag("idle_streams", "live", APP, "on", "Let players wait for a stream that is not published yet."),
        time("drop_idle_publisher", "live", APP, "", "Drop publishers that send nothing for this long."),
        B::new("meta", "codec", APP, "Metadata sent to players: on (rebuilt), copy (as published) or off.")
            .arg(a("mode", Kind::Choice(&["on", "copy", "off"])))
            .def("on")
            .0,
        // ---- access ----
        B::new("allow", "access", APP, "Allow publishing and/or playing from an address, CIDR network or all.")
            .arg(opt("scope", Kind::Choice(&["publish", "play"])))
            .arg(a("address", Kind::Text))
            .multi()
            .0,
        B::new("deny", "access", APP, "Deny publishing and/or playing from an address, CIDR network or all.")
            .arg(opt("scope", Kind::Choice(&["publish", "play"])))
            .arg(a("address", Kind::Text))
            .multi()
            .0,
        // ---- relay ----
        B::new("push", "relay", APP, "Relay published streams to another server: rtmp[s]://host[:port]/app[/name].")
            .arg(a("url", Kind::Url))
            .arg(rep("parameters", Kind::Param(RELAY_KEYS)))
            .multi()
            .0,
        B::new("pull", "relay", APP, "Pull streams from another server when played; static=1 pulls all the time.")
            .arg(a("url", Kind::Url))
            .arg(rep("parameters", Kind::Param(PULL_KEYS)))
            .multi()
            .0,
        time("push_reconnect", "relay", APP, "3s", "Delay before reconnecting a failed push."),
        time("pull_reconnect", "relay", APP, "3s", "Delay before reconnecting a failed pull."),
        time("relay_buffer", "relay", APP, "5s", "Buffer length announced to the upstream server on pull."),
        flag("session_relay", "relay", APP, "off", "Accepted for compatibility (relays are per stream)."),
        // ---- notify ----
        url("on_publish", APP, "HTTP callback before publishing; non-2xx rejects, 3xx renames or relays."),
        url("on_play", APP, "HTTP callback before playing; non-2xx rejects, 3xx renames or pulls."),
        url("on_publish_done", APP, "HTTP callback when publishing ends."),
        url("on_play_done", APP, "HTTP callback when playing ends."),
        url("on_done", APP, "HTTP callback when publishing or playing ends."),
        url("on_record_done", REC, "HTTP callback when a recording file is closed."),
        url("on_update", APP, "Periodic HTTP callback while publishing or playing; non-2xx drops the client."),
        time("notify_update_timeout", "notify", APP, "30s", "Interval of on_update (0 disables it)."),
        flag("notify_update_strict", "notify", APP, "off", "Also drop the client when on_update fails or redirects."),
        flag("notify_relay_redirect", "notify", APP, "off", "Rename streams redirected to an rtmp:// URL."),
        B::new("notify_method", "notify", APP, "HTTP method for callbacks.")
            .arg(a("method", Kind::Choice(&["get", "post"])))
            .def("post")
            .0,
        // ---- exec ----
        exec("exec_push", "Run a command for every published stream (killed when publishing stops)."),
        exec("exec", "Same as exec_push."),
        exec("exec_pull", "Run a command when a stream that is not published is played."),
        exec("exec_publish", "Run a command once when publishing starts."),
        exec("exec_publish_done", "Run a command once when publishing ends."),
        exec("exec_play", "Run a command once when playing starts."),
        exec("exec_play_done", "Run a command once when playing ends."),
        exec("exec_record_done", "Run a command when a recording file is closed ($path, $filename, ...)."),
        exec("exec_static", "Run a command while the server runs."),
        flag("respawn", "exec", APP, "on", "Restart exec_push / exec_pull commands that exit."),
        time("respawn_timeout", "exec", APP, "5s", "Delay before restarting a command."),
        B::new("exec_kill_signal", "exec", APP, "Signal used to stop commands (term, kill, int, ...).")
            .arg(a("signal", Kind::Text))
            .def("kill")
            .0,
        flag("exec_options", "exec", APP, "off", "Treat name=X arguments of exec directives as stream filters."),
        // ---- record ----
        B::new("record", "record", REC, "What to record: all, audio, video, keyframes, manual (control module) or off.")
            .arg(a("mode", Kind::Choices(&["all", "audio", "video", "keyframes", "manual", "off"])))
            .def("off")
            .0,
        B::new("record_path", "record", REC, "Directory for recordings.").arg(a("directory", Kind::Path)).0,
        B::new("record_suffix", "record", REC, "File name suffix; strftime patterns allowed (-%Y%m%d.flv).")
            .arg(a("suffix", Kind::Text))
            .def(".flv")
            .0,
        flag("record_unique", "record", REC, "off", "Add a timestamp to file names."),
        flag("record_append", "record", REC, "off", "Append to existing files."),
        flag("record_lock", "record", REC, "off", "Lock files while recording."),
        flag("record_notify", "record", REC, "off", "Send NetStream.Record.Start / Stop to the publisher."),
        B::new("record_max_size", "record", REC, "Close the file when it reaches this size.").arg(a("size", Kind::Size)).0,
        B::new("record_max_frames", "record", REC, "Close the file after this many video frames.")
            .arg(a("frames", Kind::Int))
            .0,
        time("record_interval", "record", REC, "", "Start a new file after this long."),
        B::new("recorder", "record", &["application"], "A named recorder with its own record settings.")
            .arg(a("name", Kind::Text))
            .block("recorder")
            .multi()
            .0,
        // ---- log ----
        B::new("access_log", "log", APP, "Access log file and format name, or off.")
            .arg(a("file", Kind::Path))
            .arg(opt("format", Kind::Text))
            .def("logs/access.log combined")
            .multi()
            .0,
        // ---- http ----
        B::new("server", "http", &["http"], "An HTTP server for statistics and control.").block("http_server").multi().0,
        B::new("listen", "http", &["http_server"], "Address of the HTTP server (no ssl).")
            .arg(a("address", Kind::Addr))
            .def("80")
            .multi()
            .0,
        B::new("location", "http", &["http_server"], "Settings for a URI prefix (or an exact URI with =).")
            .arg(opt("match", Kind::Choice(&["=", "^~"])))
            .arg(a("uri", Kind::Text))
            .block("location")
            .multi()
            .0,
        B::new("rtmp_stat", "stat", HTTP_LOC, "Serve RTMP statistics as XML here.")
            .arg(a("sections", Kind::Choices(&["all", "global", "live", "clients"])))
            .0,
        B::new("rtmp_stat_stylesheet", "stat", HTTP_LOC, "XSL stylesheet linked from the statistics (e.g. stat.xsl).")
            .arg(a("uri", Kind::Text))
            .0,
        B::new("rtmp_control", "control", HTTP_LOC, "Serve the control interface here (record, drop, redirect).")
            .arg(a("sections", Kind::Choices(&["all", "record", "drop", "redirect"])))
            .0,
        B::new("root", "http", HTTP_LOC, "Directory for static files such as stat.xsl.").arg(a("directory", Kind::Path)).def("html").0,
        B::new("allow", "http", HTTP_LOC, "Allow access from an address, CIDR network or all.")
            .arg(a("address", Kind::Text))
            .multi()
            .0,
        B::new("deny", "http", HTTP_LOC, "Deny access from an address, CIDR network or all.")
            .arg(a("address", Kind::Text))
            .multi()
            .0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every directive in the schema is accepted by the parser in each
    /// context it is listed for.
    #[test]
    fn schema_matches_parser() {
        let sample = |s: &Spec| -> String {
            let v: Vec<&str> = s
                .args
                .iter()
                .filter(|a| !a.opt)
                .map(|a| match &a.kind {
                    Kind::Flag => "on",
                    Kind::Time => "5s",
                    Kind::Size => "4096",
                    Kind::Int => "8",
                    Kind::Addr => "127.0.0.1:0",
                    Kind::Url => match s.module {
                        "notify" => "http://127.0.0.1/cb",
                        _ => "rtmp://127.0.0.1/app",
                    },
                    Kind::Choice(v) | Kind::Choices(v) => v[0],
                    _ if s.name == "access_log" => "off",
                    _ if s.name.starts_with("allow") || s.name.starts_with("deny") => "all",
                    _ if s.name == "exec_kill_signal" => "term",
                    _ if s.name == "log_format" => "fmt",
                    _ => "x",
                })
                .collect();
            let mut line = format!("{} {}", s.name, v.join(" "));
            if s.name == "log_format" {
                line.push_str(" $remote_addr");
            }
            match s.block {
                Some(_) => format!("{line} {{ }}"),
                None => format!("{line};"),
            }
        };
        let wrap = |ctx: &str, d: &str| -> String {
            match ctx {
                "main" => d.to_string(),
                "rtmp" => format!("rtmp {{ {d} }}"),
                "server" => format!("rtmp {{ server {{ {d} }} }}"),
                "application" => format!("rtmp {{ server {{ application a {{ {d} }} }} }}"),
                "recorder" => format!("rtmp {{ server {{ application a {{ recorder r {{ {d} }} }} }} }}"),
                "http" => format!("rtmp {{ }} http {{ {d} }}"),
                "http_server" => format!("rtmp {{ }} http {{ server {{ {d} }} }}"),
                "location" => format!("rtmp {{ }} http {{ server {{ location / {{ {d} }} }} }}"),
                _ => unreachable!("{ctx}"),
            }
        };
        for s in specs() {
            if s.name == "rtmp" {
                continue;
            }
            for ctx in s.ctx {
                let mut src = wrap(ctx, &sample(&s));
                if !src.contains("rtmp {") {
                    src.push_str(" rtmp { }");
                }
                let c = crate::config::Config::parse(&src, "t");
                assert!(c.is_ok(), "{} in {ctx}: {src}: {:?}", s.name, c.err());
                let c = c.unwrap();
                assert!(
                    !c.warnings.iter().any(|w| w.contains("not supported")),
                    "{} in {ctx} is not supported: {:?}",
                    s.name,
                    c.warnings
                );
            }
        }
    }
}
