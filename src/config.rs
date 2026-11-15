//! nginx-style configuration.
//!
//! Accepts a full `nginx.conf` (other top-level blocks such as `events` or
//! `http` are ignored) or a file containing only the `rtmp { ... }` block, so
//! existing nginx-rtmp configurations can be reused unchanged.
//!
//! Directive inheritance follows nginx: server-level directives written in
//! `rtmp {}` are defaults for every `server {}`, and application-level
//! directives written in `rtmp {}` / `server {}` are defaults for every
//! `application {}`. Unsupported directives are logged and ignored.

use crate::access_log::{LogFormat, LogTarget};
use crate::notify::{AppNotify, Method, NotifyUrl, SrvNotify};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{file}:{line}: {msg}")]
    Syntax { file: String, line: usize, msg: String },
    #[error("cannot read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("no \"rtmp\" block found")]
    NoRtmp,
}

// ---------------------------------------------------------------------------
// Generic directive tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Directive {
    pub name: String,
    pub args: Vec<String>,
    pub block: Option<Vec<Directive>>,
    pub line: usize,
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Open,
    Close,
    Semi,
}

fn tokenize(src: &str, file: &str) -> Result<Vec<(Tok, usize)>, ConfigError> {
    let err = |line, msg: &str| ConfigError::Syntax { file: file.into(), line, msg: msg.into() };
    let mut out = Vec::new();
    let mut chars = src.chars().peekable();
    let mut line = 1;
    while let Some(&c) = chars.peek() {
        match c {
            '\n' => {
                line += 1;
                chars.next();
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            '#' => {
                while let Some(&c) = chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '{' => {
                chars.next();
                out.push((Tok::Open, line));
            }
            '}' => {
                chars.next();
                out.push((Tok::Close, line));
            }
            ';' => {
                chars.next();
                out.push((Tok::Semi, line));
            }
            '"' | '\'' => {
                let q = c;
                chars.next();
                let start = line;
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => return Err(err(start, "unterminated string")),
                        Some(c) if c == q => break,
                        Some('\\') => {
                            if let Some(n) = chars.next() {
                                s.push(n);
                            }
                        }
                        Some(c) => {
                            if c == '\n' {
                                line += 1;
                            }
                            s.push(c);
                        }
                    }
                }
                out.push((Tok::Word(s), start));
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, '{' | '}' | ';' | '#') {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                out.push((Tok::Word(s), line));
            }
        }
    }
    Ok(out)
}

pub fn parse_tree(src: &str, file: &str) -> Result<Vec<Directive>, ConfigError> {
    let toks = tokenize(src, file)?;
    let mut pos = 0;
    let tree = parse_block(&toks, &mut pos, file, false)?;
    Ok(tree)
}

fn parse_block(
    toks: &[(Tok, usize)],
    pos: &mut usize,
    file: &str,
    nested: bool,
) -> Result<Vec<Directive>, ConfigError> {
    let err = |line, msg: String| ConfigError::Syntax { file: file.into(), line, msg };
    let mut out = Vec::new();
    loop {
        let Some((tok, line)) = toks.get(*pos) else {
            if nested {
                let l = toks.last().map(|t| t.1).unwrap_or(0);
                return Err(err(l, "unexpected end of file, expecting \"}\"".into()));
            }
            return Ok(out);
        };
        match tok {
            Tok::Close if nested => {
                *pos += 1;
                return Ok(out);
            }
            Tok::Word(name) => {
                let line = *line;
                let name = name.clone();
                *pos += 1;
                let mut args = Vec::new();
                loop {
                    match toks.get(*pos) {
                        Some((Tok::Word(w), _)) => {
                            args.push(w.clone());
                            *pos += 1;
                        }
                        Some((Tok::Semi, _)) => {
                            *pos += 1;
                            out.push(Directive { name, args, block: None, line });
                            break;
                        }
                        Some((Tok::Open, _)) => {
                            *pos += 1;
                            let block = parse_block(toks, pos, file, true)?;
                            out.push(Directive { name, args, block: Some(block), line });
                            break;
                        }
                        _ => return Err(err(line, format!("directive \"{name}\" is not terminated by \";\""))),
                    }
                }
            }
            other => return Err(err(*line, format!("unexpected {other:?}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Typed configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessAction {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessScope {
    Publish,
    Play,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessTarget {
    All,
    Net { addr: IpAddr, prefix: u8 },
}

/// `allow` / `deny` rule (`ngx_rtmp_access_module.c`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRule {
    pub action: AccessAction,
    pub scope: AccessScope,
    pub target: AccessTarget,
}

impl AccessRule {
    pub fn matches(&self, ip: IpAddr) -> bool {
        match &self.target {
            AccessTarget::All => true,
            AccessTarget::Net { addr, prefix } => match (addr, ip) {
                (IpAddr::V4(a), IpAddr::V4(b)) => {
                    let m = if *prefix == 0 { 0 } else { u32::MAX << (32 - *prefix as u32) };
                    (u32::from(*a) & m) == (u32::from(b) & m)
                }
                (IpAddr::V6(a), IpAddr::V6(b)) => {
                    let m = if *prefix == 0 { 0 } else { u128::MAX << (128 - *prefix as u32) };
                    (u128::from(*a) & m) == (u128::from(b) & m)
                }
                (IpAddr::V6(a), IpAddr::V4(b)) => a.to_ipv4_mapped() == Some(b) && *prefix >= 96,
                (IpAddr::V4(_), IpAddr::V6(b)) => match b.to_ipv4_mapped() {
                    Some(b4) => self.matches(IpAddr::V4(b4)),
                    None => false,
                },
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaMode {
    On,
    Copy,
    Off,
}

/// `access_log` setting at one level (`ngx_rtmp_log_app_conf_t`). A level
/// with its own `access_log` directives replaces the inherited ones.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum AccessLogConf {
    /// No directive at any level: nginx logs to `logs/access.log`.
    #[default]
    Unset,
    Off,
    /// (path as written, format name)
    Logs(Vec<(String, String)>),
}

#[derive(Debug, Clone)]
pub struct AppConf {
    pub name: String,
    // live module
    pub live: bool,
    pub sync: Duration,
    pub interleave: bool,
    pub wait_key: bool,
    pub wait_video: bool,
    pub publish_notify: bool,
    pub play_restart: bool,
    pub idle_streams: bool,
    pub drop_idle_publisher: Option<Duration>,
    // codec module
    pub meta: MetaMode,
    // access module
    pub access: Vec<AccessRule>,
    // relay module
    pub pushes: Vec<Arc<RelayTarget>>,
    pub pulls: Vec<Arc<RelayTarget>>,
    pub static_pulls: Vec<Arc<RelayTarget>>,
    pub push_reconnect: Duration,
    pub pull_reconnect: Duration,
    /// `relay_buffer`: buffer length announced to the upstream on pull.
    pub relay_buffer: Duration,
    // notify module
    pub notify: AppNotify,
    // log module
    pub access_log: AccessLogConf,
    /// Opened targets for `access_log` (filled by `access_log::open_all`).
    pub access_logs: Vec<LogTarget>,
}

impl Default for AppConf {
    fn default() -> Self {
        Self {
            name: String::new(),
            live: false,
            sync: Duration::from_millis(300),
            interleave: false,
            wait_key: true,
            wait_video: false,
            publish_notify: false,
            play_restart: false,
            idle_streams: true,
            drop_idle_publisher: None,
            meta: MetaMode::On,
            access: Vec::new(),
            pushes: Vec::new(),
            pulls: Vec::new(),
            static_pulls: Vec::new(),
            push_reconnect: Duration::from_secs(3),
            pull_reconnect: Duration::from_secs(3),
            relay_buffer: Duration::from_secs(5),
            notify: AppNotify::new(),
            access_log: AccessLogConf::Unset,
            access_logs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConf {
    pub listen: Vec<SocketAddr>,
    /// For each `listen` entry, whether it is marked `ssl` (RTMPS).
    pub listen_ssl: Vec<bool>,
    /// `ssl_certificate` / `ssl_certificate_key` (PEM), for `listen ... ssl`.
    pub ssl_certificate: Option<String>,
    pub ssl_certificate_key: Option<String>,
    pub timeout: Duration,
    pub ping: Duration,
    pub ping_timeout: Duration,
    pub max_streams: usize,
    pub ack_window: u32,
    pub chunk_size: usize,
    pub max_message: usize,
    pub out_queue: usize,
    pub out_cork: Option<usize>,
    pub play_time_fix: bool,
    pub publish_time_fix: bool,
    pub busy: bool,
    pub buflen: Duration,
    pub so_keepalive: bool,
    /// `on_connect` / `on_disconnect`.
    pub notify: SrvNotify,
    /// `netcall_timeout`: limit for each HTTP callback.
    pub netcall_timeout: Duration,
    /// `access_log` for connections that never select an application.
    pub access_log: AccessLogConf,
    pub access_logs: Vec<LogTarget>,
    pub applications: Vec<AppConf>,
}

impl Default for ServerConf {
    fn default() -> Self {
        Self {
            listen: Vec::new(),
            listen_ssl: Vec::new(),
            ssl_certificate: None,
            ssl_certificate_key: None,
            timeout: Duration::from_secs(60),
            ping: Duration::from_secs(60),
            ping_timeout: Duration::from_secs(30),
            max_streams: 32,
            ack_window: 5_000_000,
            chunk_size: 4096,
            max_message: 1024 * 1024,
            out_queue: 256,
            out_cork: None,
            play_time_fix: true,
            publish_time_fix: true,
            busy: false,
            buflen: Duration::from_secs(1),
            so_keepalive: false,
            notify: SrvNotify::default(),
            netcall_timeout: Duration::from_secs(10),
            access_log: AccessLogConf::Unset,
            access_logs: Vec::new(),
            applications: Vec::new(),
        }
    }
}

impl ServerConf {
    pub fn app(&self, name: &str) -> Option<&AppConf> {
        self.applications.iter().find(|a| a.name == name)
    }
    pub fn out_cork(&self) -> usize {
        self.out_cork.unwrap_or(self.out_queue / 8)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub servers: Vec<ServerConf>,
    /// `log_format` definitions, `combined` first.
    pub log_formats: Vec<(String, LogFormat)>,
    /// Directory relative paths (log files) are resolved against, like
    /// nginx's `-p` prefix. `None` resolves against the working directory
    /// and disables the implicit `logs/access.log`.
    pub prefix: Option<PathBuf>,
    /// Top-level `error_log file [level]` (only the first one is used).
    pub error_log: Option<ErrorLogConf>,
    /// Top-level `pid file`.
    pub pid: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorLogConf {
    pub path: String,
    /// nginx level name: debug, info, notice, warn, error, crit, alert, emerg.
    pub level: String,
}

pub const ERROR_LOG_LEVELS: [&str; 8] = ["debug", "info", "notice", "warn", "error", "crit", "alert", "emerg"];

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(path.display().to_string(), e))?;
        Self::parse(&src, &path.display().to_string())
    }

    pub fn parse(src: &str, file: &str) -> Result<Self, ConfigError> {
        let tree = parse_tree(src, file)?;
        let mut cx = Cx {
            file,
            warnings: Vec::new(),
            formats: vec![("combined".to_string(), LogFormat::combined())],
        };
        let mut error_log = None;
        let mut pid = None;
        for d in &tree {
            match d.name.as_str() {
                "error_log" if error_log.is_none() => {
                    let (path, level) = match d.args.as_slice() {
                        [p] => (p.clone(), "error".to_string()),
                        [p, l, ..] => (p.clone(), l.clone()),
                        _ => return Err(cx.err(d.line, "invalid number of arguments in \"error_log\"")),
                    };
                    if !ERROR_LOG_LEVELS.contains(&level.as_str()) {
                        return Err(cx.err(d.line, format!("invalid log level \"{level}\"")));
                    }
                    error_log = Some(ErrorLogConf { path, level });
                }
                "pid" => pid = Some(cx.one(d)?.to_string()),
                _ => {}
            }
        }
        let rtmp = tree
            .iter()
            .find(|d| d.name == "rtmp" && d.block.is_some())
            .ok_or(ConfigError::NoRtmp)?;
        let rtmp_block = rtmp.block.as_ref().unwrap();

        // Defaults from rtmp{} level.
        let mut srv_defaults = ServerConf::default();
        let mut app_defaults = AppConf::default();
        for d in rtmp_block.iter().filter(|d| d.name != "server") {
            cx.apply_any(d, Some(&mut srv_defaults), &mut app_defaults)?;
        }

        let mut servers = Vec::new();
        for s in rtmp_block.iter().filter(|d| d.name == "server") {
            let block = s.block.as_ref().ok_or_else(|| cx.err(s.line, "\"server\" needs a block"))?;
            let mut srv = srv_defaults.clone();
            let mut app_def = app_defaults.clone();
            let inherited_log = std::mem::take(&mut app_def.access_log);
            for d in block.iter().filter(|d| d.name != "application") {
                cx.apply_any(d, Some(&mut srv), &mut app_def)?;
            }
            if app_def.access_log == AccessLogConf::Unset {
                app_def.access_log = inherited_log;
            }
            srv.access_log = app_def.access_log.clone();
            for a in block.iter().filter(|d| d.name == "application") {
                let name = a.args.first().ok_or_else(|| cx.err(a.line, "\"application\" needs a name"))?;
                let ablock = a.block.as_ref().ok_or_else(|| cx.err(a.line, "\"application\" needs a block"))?;
                let mut app = app_def.clone();
                app.name = name.clone();
                // access rules accumulate in order: inherited only if none set locally
                let inherited_access = std::mem::take(&mut app.access);
                let inherited_log = std::mem::take(&mut app.access_log);
                for d in ablock {
                    cx.apply_any(d, None, &mut app)?;
                }
                if app.access.is_empty() {
                    app.access = inherited_access;
                }
                if app.access_log == AccessLogConf::Unset {
                    app.access_log = inherited_log;
                }
                srv.applications.push(app);
            }
            if srv.listen.is_empty() {
                srv.listen.push("0.0.0.0:1935".parse().unwrap());
                srv.listen_ssl.push(false);
            }
            if let Some(addr) = srv.listen.iter().zip(&srv.listen_ssl).find_map(|(a, &ssl)| ssl.then_some(a)) {
                if srv.ssl_certificate.is_none() {
                    return Err(cx.err(s.line, format!("no \"ssl_certificate\" is defined for the \"listen {addr} ssl\"")));
                }
                if srv.ssl_certificate_key.is_none() {
                    return Err(cx.err(s.line, format!("no \"ssl_certificate_key\" is defined for the \"listen {addr} ssl\"")));
                }
            }
            servers.push(srv);
        }
        for w in cx.warnings {
            tracing::warn!("{w}");
        }
        Ok(Config { servers, log_formats: cx.formats, prefix: None, error_log, pid })
    }
}

struct Cx<'a> {
    file: &'a str,
    warnings: Vec<String>,
    formats: Vec<(String, LogFormat)>,
}

impl Cx<'_> {
    fn err(&self, line: usize, msg: impl Into<String>) -> ConfigError {
        ConfigError::Syntax { file: self.file.into(), line, msg: msg.into() }
    }

    fn one<'d>(&self, d: &'d Directive) -> Result<&'d str, ConfigError> {
        match d.args.as_slice() {
            [a] => Ok(a),
            _ => Err(self.err(d.line, format!("invalid number of arguments in \"{}\"", d.name))),
        }
    }

    fn flag(&self, d: &Directive) -> Result<bool, ConfigError> {
        match self.one(d)? {
            "on" => Ok(true),
            "off" => Ok(false),
            v => Err(self.err(d.line, format!("invalid value \"{v}\" in \"{}\", it must be \"on\" or \"off\"", d.name))),
        }
    }

    fn time(&self, d: &Directive) -> Result<Duration, ConfigError> {
        let v = self.one(d)?;
        parse_time(v).ok_or_else(|| self.err(d.line, format!("invalid time \"{v}\" in \"{}\"", d.name)))
    }

    fn size(&self, d: &Directive) -> Result<usize, ConfigError> {
        let v = self.one(d)?;
        parse_size(v).ok_or_else(|| self.err(d.line, format!("invalid size \"{v}\" in \"{}\"", d.name)))
    }

    /// Apply a directive at any level. `srv` is `None` inside `application`.
    fn apply_any(
        &mut self,
        d: &Directive,
        srv: Option<&mut ServerConf>,
        app: &mut AppConf,
    ) -> Result<(), ConfigError> {
        if d.name == "log_format" {
            // nginx only honours it at rtmp{} level (with a warning
            // elsewhere); formats are global either way.
            let [name, parts @ ..] = d.args.as_slice() else {
                return Err(self.err(d.line, "invalid number of arguments in \"log_format\""));
            };
            if parts.is_empty() {
                return Err(self.err(d.line, "invalid number of arguments in \"log_format\""));
            }
            if self.formats.iter().any(|(n, _)| n == name) {
                return Err(self.err(d.line, format!("duplicate \"log_format\" name \"{name}\"")));
            }
            let f = LogFormat::compile(parts).map_err(|e| self.err(d.line, e))?;
            self.formats.push((name.clone(), f));
            return Ok(());
        }
        if d.name == "notify_method" {
            // Applies to both the app callbacks and on_connect/on_disconnect.
            let m = match self.one(d)?.to_ascii_lowercase().as_str() {
                "get" => Method::Get,
                "post" => Method::Post,
                _ => return Err(self.err(d.line, "got unexpected method in \"notify_method\"")),
            };
            app.notify.method = m;
            if let Some(srv) = srv {
                srv.notify.method = m;
            }
            return Ok(());
        }
        if let Some(srv) = srv
            && self.apply_srv(d, srv)? {
                return Ok(());
            }
        if self.apply_app(d, app)? {
            return Ok(());
        }
        self.warnings.push(format!(
            "{}:{}: directive \"{}\" is not supported yet, ignored",
            self.file, d.line, d.name
        ));
        Ok(())
    }

    fn apply_srv(&mut self, d: &Directive, s: &mut ServerConf) -> Result<bool, ConfigError> {
        match d.name.as_str() {
            "listen" => {
                let v = d.args.first().ok_or_else(|| self.err(d.line, "\"listen\" needs an address"))?;
                let addr = parse_listen(v).ok_or_else(|| self.err(d.line, format!("invalid listen \"{v}\"")))?;
                let mut ssl = false;
                for extra in &d.args[1..] {
                    if extra == "ssl" {
                        ssl = true;
                    } else if !matches!(extra.as_str(), "bind" | "ipv6only=on" | "ipv6only=off")
                        && !extra.starts_with("so_keepalive")
                    {
                        self.warnings.push(format!("{}:{}: listen option \"{extra}\" ignored", self.file, d.line));
                    }
                }
                s.listen.push(addr);
                s.listen_ssl.push(ssl);
            }
            "timeout" => s.timeout = self.time(d)?,
            "ping" => s.ping = self.time(d)?,
            "ping_timeout" => s.ping_timeout = self.time(d)?,
            "max_streams" => s.max_streams = self.size(d)?,
            "ack_window" => s.ack_window = self.size(d)? as u32,
            "chunk_size" => {
                let v = self.size(d)?;
                if v < 128 {
                    return Err(self.err(d.line, "chunk_size must be at least 128"));
                }
                s.chunk_size = v;
            }
            "max_message" => s.max_message = self.size(d)?,
            "out_queue" => s.out_queue = self.size(d)?,
            "out_cork" => s.out_cork = Some(self.size(d)?),
            "play_time_fix" => s.play_time_fix = self.flag(d)?,
            "publish_time_fix" => s.publish_time_fix = self.flag(d)?,
            "busy" => s.busy = self.flag(d)?,
            "buflen" => s.buflen = self.time(d)?,
            "so_keepalive" => s.so_keepalive = self.flag(d)?,
            "ssl_certificate" => s.ssl_certificate = Some(self.one(d)?.to_string()),
            "ssl_certificate_key" => s.ssl_certificate_key = Some(self.one(d)?.to_string()),
            "on_connect" => s.notify.on_connect = Some(self.notify_url(d)?),
            "on_disconnect" => s.notify.on_disconnect = Some(self.notify_url(d)?),
            "netcall_timeout" => s.netcall_timeout = self.time(d)?,
            "netcall_buffer" => {
                self.size(d)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn apply_app(&mut self, d: &Directive, a: &mut AppConf) -> Result<bool, ConfigError> {
        match d.name.as_str() {
            "live" => a.live = self.flag(d)?,
            "sync" => a.sync = self.time(d)?,
            "interleave" => a.interleave = self.flag(d)?,
            "wait_key" => a.wait_key = self.flag(d)?,
            "wait_video" => a.wait_video = self.flag(d)?,
            "publish_notify" => a.publish_notify = self.flag(d)?,
            "play_restart" => a.play_restart = self.flag(d)?,
            "idle_streams" => a.idle_streams = self.flag(d)?,
            "drop_idle_publisher" => a.drop_idle_publisher = Some(self.time(d)?),
            "access_log" => match d.args.as_slice() {
                [off] if off == "off" => a.access_log = AccessLogConf::Off,
                [path] | [path, _] => {
                    let fmt = d.args.get(1).map_or("combined", String::as_str);
                    if !self.formats.iter().any(|(n, _)| n == fmt) {
                        return Err(self.err(d.line, format!("unknown log format \"{fmt}\"")));
                    }
                    let entry = (path.clone(), fmt.to_string());
                    match &mut a.access_log {
                        AccessLogConf::Off => {}
                        AccessLogConf::Logs(v) => v.push(entry),
                        AccessLogConf::Unset => a.access_log = AccessLogConf::Logs(vec![entry]),
                    }
                }
                _ => return Err(self.err(d.line, "invalid number of arguments in \"access_log\"")),
            },
            "stream_buckets" | "buffer" => {} // tuning knobs with no equivalent; accepted silently
            "meta" => {
                a.meta = match self.one(d)? {
                    "on" => MetaMode::On,
                    "copy" => MetaMode::Copy,
                    "off" => MetaMode::Off,
                    v => return Err(self.err(d.line, format!("invalid meta \"{v}\""))),
                }
            }
            "allow" | "deny" => a.access.push(self.access_rule(d)?),
            "push" | "pull" => {
                let t = self.relay_target(d)?;
                match (d.name.as_str(), t.is_static) {
                    ("pull", true) => a.static_pulls.push(Arc::new(t)),
                    ("pull", false) => a.pulls.push(Arc::new(t)),
                    _ => a.pushes.push(Arc::new(t)),
                }
            }
            "push_reconnect" => a.push_reconnect = self.time(d)?,
            "pull_reconnect" => a.pull_reconnect = self.time(d)?,
            "relay_buffer" => a.relay_buffer = self.time(d)?,
            "session_relay" => {
                self.flag(d)?;
            }
            "on_play" => a.notify.on_play = Some(self.notify_url(d)?),
            "on_publish" => a.notify.on_publish = Some(self.notify_url(d)?),
            "on_play_done" => a.notify.on_play_done = Some(self.notify_url(d)?),
            "on_publish_done" => a.notify.on_publish_done = Some(self.notify_url(d)?),
            "on_done" => a.notify.on_done = Some(self.notify_url(d)?),
            "on_record_done" => a.notify.on_record_done = Some(self.notify_url(d)?),
            "on_update" => a.notify.on_update = Some(self.notify_url(d)?),
            "notify_update_timeout" => a.notify.update_timeout = self.time(d)?,
            "notify_update_strict" => a.notify.update_strict = self.flag(d)?,
            "notify_relay_redirect" => a.notify.relay_redirect = self.flag(d)?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn notify_url(&self, d: &Directive) -> Result<Arc<NotifyUrl>, ConfigError> {
        let v = self.one(d)?;
        NotifyUrl::parse(v)
            .map(Arc::new)
            .ok_or_else(|| self.err(d.line, format!("invalid url \"{v}\" in \"{}\"", d.name)))
    }

    fn relay_target(&self, d: &Directive) -> Result<RelayTarget, ConfigError> {
        let url = d.args.first().ok_or_else(|| self.err(d.line, format!("\"{}\" needs a URL", d.name)))?;
        let mut t = RelayTarget::parse_url(url)
            .ok_or_else(|| self.err(d.line, format!("invalid relay URL \"{url}\"")))?;
        for arg in &d.args[1..] {
            let (k, v) = arg.split_once('=').unwrap_or((arg.as_str(), "1"));
            let num = || v.parse::<i64>().map_err(|_| self.err(d.line, format!("invalid number in \"{arg}\"")));
            match k.to_ascii_lowercase().as_str() {
                "app" => t.app = v.to_string(),
                "name" => t.name = v.to_string(),
                "tcurl" => t.tc_url = v.to_string(),
                "pageurl" => t.page_url = v.to_string(),
                "swfurl" => t.swf_url = v.to_string(),
                "flashver" => t.flash_ver = v.to_string(),
                "playpath" => t.play_path = v.to_string(),
                "live" => t.live = num()? != 0,
                "start" => t.start = num()?,
                "stop" => t.stop = num()?,
                "static" => t.is_static = num()? != 0,
                _ => return Err(self.err(d.line, format!("unsupported parameter \"{arg}\""))),
            }
        }
        if t.is_static {
            if d.name != "pull" {
                return Err(self.err(d.line, "static push is not allowed"));
            }
            if t.name.is_empty() {
                return Err(self.err(d.line, "stream name missing in static pull declaration"));
            }
        }
        Ok(t)
    }

    fn access_rule(&self, d: &Directive) -> Result<AccessRule, ConfigError> {
        let action = if d.name == "allow" { AccessAction::Allow } else { AccessAction::Deny };
        let (scope, target) = match d.args.as_slice() {
            [t] => (AccessScope::Both, t),
            [s, t] => (
                match s.as_str() {
                    "publish" => AccessScope::Publish,
                    "play" => AccessScope::Play,
                    _ => return Err(self.err(d.line, format!("unexpected \"{s}\" in \"{}\"", d.name))),
                },
                t,
            ),
            _ => return Err(self.err(d.line, format!("invalid number of arguments in \"{}\"", d.name))),
        };
        let target = if target == "all" {
            AccessTarget::All
        } else {
            let (ip, prefix) = match target.split_once('/') {
                Some((ip, p)) => (ip, Some(p)),
                None => (target.as_str(), None),
            };
            let addr: IpAddr = ip.parse().map_err(|_| self.err(d.line, format!("invalid address \"{target}\"")))?;
            let max = if addr.is_ipv4() { 32 } else { 128 };
            let prefix = match prefix {
                Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max)
                    .ok_or_else(|| self.err(d.line, format!("invalid mask in \"{target}\"")))?,
                None => max,
            };
            AccessTarget::Net { addr, prefix }
        };
        Ok(AccessRule { action, scope, target })
    }
}

/// `push` / `pull` target (`ngx_rtmp_relay_target_t`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RelayTarget {
    /// Original URL as written (without `rtmp://`), used in logs and tcUrl.
    pub url: String,
    pub host: String,
    pub port: u16,
    /// Remote application (from `app=` or the first URL path segment).
    pub app: String,
    /// Remote stream name (from `playPath=` or the rest of the URL path).
    pub play_path: String,
    /// `name=`: only relay this local stream (and local name for static pulls).
    pub name: String,
    pub tc_url: String,
    pub page_url: String,
    pub swf_url: String,
    pub flash_ver: String,
    pub live: bool,
    pub start: i64,
    pub stop: i64,
    pub is_static: bool,
    /// `rtmps://`: connect over TLS.
    pub tls: bool,
}

/// Split a `rtmp://` / `rtmps://` scheme off `url`: (TLS, rest). A URL
/// without a scheme is plain RTMP.
pub fn split_scheme(url: &str) -> (bool, &str) {
    let lower = |n: usize| url.get(..n).map(str::to_ascii_lowercase);
    if lower(8).as_deref() == Some("rtmps://") {
        (true, &url[8..])
    } else if lower(7).as_deref() == Some("rtmp://") {
        (false, &url[7..])
    } else {
        (false, url)
    }
}

/// Whether `url` starts with `rtmp://` or `rtmps://`.
pub fn is_rtmp_url(url: &str) -> bool {
    split_scheme(url).1.len() != url.len()
}

impl RelayTarget {
    /// Parse `[rtmp[s]://]host[:port][/app[/play/path]]`. The default port
    /// is 1935, or 443 for `rtmps://`.
    pub fn parse_url(url: &str) -> Option<Self> {
        let (tls, rest) = split_scheme(url);
        let default_port = if tls { 443 } else { 1935 };
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
            let (h, after) = h.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None if after.is_empty() => default_port,
                None => return None,
            };
            (h.to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()?),
                None => (hostport.to_string(), default_port),
            }
        };
        if host.is_empty() {
            return None;
        }
        let (app, play_path) = match path.split_once('/') {
            Some((a, p)) => (a.to_string(), p.to_string()),
            None => (path.to_string(), String::new()),
        };
        Some(RelayTarget { url: rest.to_string(), host, port, app, play_path, tls, ..Default::default() })
    }

    /// `name=` filter.
    pub fn matches(&self, stream: &str) -> bool {
        self.name.is_empty() || self.name == stream
    }
}

/// nginx time syntax: `500ms`, `30s`, `1m`, `1h`, `1d`, combinations like
/// `1m30s`; a bare number means seconds.
pub fn parse_time(v: &str) -> Option<Duration> {
    let mut total_ms: u64 = 0;
    let mut num = String::new();
    let mut chars = v.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        let mult = match c {
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                1
            }
            'm' => 60_000,
            's' => 1000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            'w' => 604_800_000,
            'M' => 2_592_000_000,
            'y' => 31_536_000_000,
            _ => return None,
        };
        total_ms += n * mult;
        any = true;
    }
    if !num.is_empty() {
        total_ms += num.parse::<u64>().ok()? * 1000;
        any = true;
    }
    any.then(|| Duration::from_millis(total_ms))
}

/// nginx size syntax: `4096`, `8k`, `1m`, `1g` (case-insensitive).
pub fn parse_size(v: &str) -> Option<usize> {
    let (digits, mult) = match v.chars().last()? {
        'k' | 'K' => (&v[..v.len() - 1], 1024),
        'm' | 'M' => (&v[..v.len() - 1], 1024 * 1024),
        'g' | 'G' => (&v[..v.len() - 1], 1024 * 1024 * 1024),
        _ => (v, 1),
    };
    digits.parse::<usize>().ok().map(|n| n * mult)
}

fn parse_listen(v: &str) -> Option<SocketAddr> {
    if let Ok(port) = v.parse::<u16>() {
        return Some(SocketAddr::from(([0, 0, 0, 0], port)));
    }
    if let Ok(a) = v.parse::<SocketAddr>() {
        return Some(a);
    }
    // "*:1935", "localhost:1935", or a bare host meaning port 1935
    let (host, port) = match v.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') || h.starts_with('[') => (h, p.parse().ok()?),
        _ => (v, 1935),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let ip: IpAddr = match host {
        "*" => [0, 0, 0, 0].into(),
        "localhost" => [127, 0, 0, 1].into(),
        h => h.parse().ok()?,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
worker_processes 1;
events { worker_connections 1024; }

rtmp {
    chunk_size 8192;
    wait_key off;           # app-level default from rtmp{}
    server {
        listen 1935;
        listen 127.0.0.1:19350;
        timeout 10s;
        application live {
            live on;
            allow publish 127.0.0.1;
            deny publish all;
            allow play all;
            record all;     # unsupported: warned, ignored
        }
        application vod {
            play /var/flvs;
            wait_key on;
        }
    }
}

http { server { listen 8080; location / { root html; } } }
"#;

    #[test]
    fn parse_sample() {
        let c = Config::parse(SAMPLE, "test.conf").unwrap();
        assert_eq!(c.servers.len(), 1);
        let s = &c.servers[0];
        assert_eq!(s.listen.len(), 2);
        assert_eq!(s.listen[1], "127.0.0.1:19350".parse().unwrap());
        assert_eq!(s.chunk_size, 8192);
        assert_eq!(s.timeout, Duration::from_secs(10));
        let live = s.app("live").unwrap();
        assert!(live.live);
        assert!(!live.wait_key);
        assert_eq!(live.access.len(), 3);
        assert_eq!(live.access[1].action, AccessAction::Deny);
        assert!(s.app("vod").unwrap().wait_key);
        assert!(!s.app("vod").unwrap().live);
    }

    #[test]
    fn relay_directives() {
        let c = Config::parse(
            r#"rtmp { server { application a {
                push rtmp://example.com:1936/app/sub/key live=1 flashVer=x;
                push 10.0.0.1;
                pull rtmp://[::1]/src name=cam static;
                pull rtmp://up/live;
                push_reconnect 1s;
            } } }"#,
            "t",
        )
        .unwrap();
        let a = c.servers[0].app("a").unwrap();
        assert_eq!(a.pushes.len(), 2);
        let p = &a.pushes[0];
        assert_eq!((p.host.as_str(), p.port, p.app.as_str()), ("example.com", 1936, "app"));
        assert_eq!(p.play_path, "sub/key");
        assert!(p.live);
        assert_eq!(p.flash_ver, "x");
        assert_eq!((a.pushes[1].port, a.pushes[1].app.as_str()), (1935, ""));
        assert_eq!(a.static_pulls[0].host, "::1");
        assert_eq!(a.static_pulls[0].name, "cam");
        assert_eq!(a.pulls[0].app, "live");
        assert_eq!(a.push_reconnect, Duration::from_secs(1));
        assert!(Config::parse("rtmp{server{application a{push rtmp://h/a static;}}}", "t").is_err());
        assert!(Config::parse("rtmp{server{application a{pull rtmp://h/a static;}}}", "t").is_err());
        assert!(Config::parse("rtmp{server{application a{push rtmp://h/a bogus=1;}}}", "t").is_err());
    }

    #[test]
    fn time_and_size() {
        assert_eq!(parse_time("60"), Some(Duration::from_secs(60)));
        assert_eq!(parse_time("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_time("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_time("x"), None);
        assert_eq!(parse_size("8k"), Some(8192));
        assert_eq!(parse_size("1M"), Some(1 << 20));
    }

    #[test]
    fn cidr_matching() {
        let r = AccessRule {
            action: AccessAction::Allow,
            scope: AccessScope::Both,
            target: AccessTarget::Net { addr: "192.168.1.0".parse().unwrap(), prefix: 24 },
        };
        assert!(r.matches("192.168.1.77".parse().unwrap()));
        assert!(!r.matches("192.168.2.1".parse().unwrap()));
        assert!(r.matches("::ffff:192.168.1.5".parse().unwrap()));
    }

    #[test]
    fn syntax_error_reports_line() {
        let e = Config::parse("rtmp {\n server {\n listen 1935\n }\n}", "x").unwrap_err();
        assert!(e.to_string().contains("x:3"), "{e}");
    }

    #[test]
    fn main_level_directives() {
        let c = Config::parse("pid run/x.pid;
error_log logs/e.log info;
error_log other.log;
rtmp { server { } }", "t").unwrap();
        assert_eq!(c.pid.as_deref(), Some("run/x.pid"));
        assert_eq!(c.error_log, Some(ErrorLogConf { path: "logs/e.log".into(), level: "info".into() }));
        let c = Config::parse("error_log logs/e.log;
rtmp { server { } }", "t").unwrap();
        assert_eq!(c.error_log.unwrap().level, "error");
        assert!(Config::parse("error_log e.log loud;
rtmp { server { } }", "t").is_err());
        let c = Config::parse("rtmp { server { } }", "t").unwrap();
        assert!(c.pid.is_none() && c.error_log.is_none());
    }

    #[test]
    fn access_log_inheritance() {
        let c = Config::parse(
            r#"
rtmp {
    log_format short '$remote_addr $command' ' $app';
    access_log logs/rtmp.log;
    server {
        listen 1935;
        application a { live on; }
        application b { access_log /tmp/b.log short; access_log x.log; }
        application c { access_log off; }
    }
    server {
        listen 1936;
        access_log off;
        application d { live on; }
    }
}
"#,
            "t",
        )
        .unwrap();
        let s = &c.servers[0];
        let rtmp = AccessLogConf::Logs(vec![("logs/rtmp.log".into(), "combined".into())]);
        assert_eq!(s.access_log, rtmp);
        assert_eq!(s.app("a").unwrap().access_log, rtmp);
        assert_eq!(
            s.app("b").unwrap().access_log,
            AccessLogConf::Logs(vec![("/tmp/b.log".into(), "short".into()), ("x.log".into(), "combined".into())])
        );
        assert_eq!(s.app("c").unwrap().access_log, AccessLogConf::Off);
        assert_eq!(c.servers[1].app("d").unwrap().access_log, AccessLogConf::Off);
        assert_eq!(c.log_formats.len(), 2);

        let unset = Config::parse("rtmp { server { application a {} } }", "t").unwrap();
        assert_eq!(unset.servers[0].app("a").unwrap().access_log, AccessLogConf::Unset);

        for bad in [
            "rtmp { access_log a.log nope; server {} }",
            "rtmp { log_format combined '$app'; server {} }",
            "rtmp { log_format x '$bogus'; server {} }",
            "rtmp { access_log; server {} }",
        ] {
            assert!(Config::parse(bad, "t").is_err(), "{bad}");
        }
    }
}
