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

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConf {
    pub listen: Vec<SocketAddr>,
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
    pub applications: Vec<AppConf>,
}

impl Default for ServerConf {
    fn default() -> Self {
        Self {
            listen: Vec::new(),
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
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(path.display().to_string(), e))?;
        Self::parse(&src, &path.display().to_string())
    }

    pub fn parse(src: &str, file: &str) -> Result<Self, ConfigError> {
        let tree = parse_tree(src, file)?;
        let mut cx = Cx { file, warnings: Vec::new() };
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
            for d in block.iter().filter(|d| d.name != "application") {
                cx.apply_any(d, Some(&mut srv), &mut app_def)?;
            }
            for a in block.iter().filter(|d| d.name == "application") {
                let name = a.args.first().ok_or_else(|| cx.err(a.line, "\"application\" needs a name"))?;
                let ablock = a.block.as_ref().ok_or_else(|| cx.err(a.line, "\"application\" needs a block"))?;
                let mut app = app_def.clone();
                app.name = name.clone();
                // access rules accumulate in order: inherited only if none set locally
                let inherited_access = std::mem::take(&mut app.access);
                for d in ablock {
                    cx.apply_any(d, None, &mut app)?;
                }
                if app.access.is_empty() {
                    app.access = inherited_access;
                }
                srv.applications.push(app);
            }
            if srv.listen.is_empty() {
                srv.listen.push("0.0.0.0:1935".parse().unwrap());
            }
            servers.push(srv);
        }
        for w in cx.warnings {
            tracing::warn!("{w}");
        }
        Ok(Config { servers })
    }
}

struct Cx<'a> {
    file: &'a str,
    warnings: Vec<String>,
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
                for extra in &d.args[1..] {
                    if !matches!(extra.as_str(), "bind" | "ipv6only=on" | "ipv6only=off") && !extra.starts_with("so_keepalive") {
                        self.warnings.push(format!("{}:{}: listen option \"{extra}\" ignored", self.file, d.line));
                    }
                }
                s.listen.push(addr);
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
            _ => return Ok(false),
        }
        Ok(true)
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
}

impl RelayTarget {
    /// Parse `[rtmp://]host[:port][/app[/play/path]]`.
    pub fn parse_url(url: &str) -> Option<Self> {
        let rest = if url.len() >= 7 && url[..7].eq_ignore_ascii_case("rtmp://") { &url[7..] } else { url };
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
            let (h, after) = h.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None if after.is_empty() => 1935,
                None => return None,
            };
            (h.to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()?),
                None => (hostport.to_string(), 1935),
            }
        };
        if host.is_empty() {
            return None;
        }
        let (app, play_path) = match path.split_once('/') {
            Some((a, p)) => (a.to_string(), p.to_string()),
            None => (path.to_string(), String::new()),
        };
        Some(RelayTarget { url: rest.to_string(), host, port, app, play_path, ..Default::default() })
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
}
