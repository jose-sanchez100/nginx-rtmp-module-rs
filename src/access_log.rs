//! Access log (`ngx_rtmp_log_module.c`).
//!
//! One line is written per client connection when it closes, using either
//! the built-in `combined` format or one declared with `log_format`. Relay
//! connections are not logged, as in nginx.

use crate::config::{AccessLogConf, Config};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// `ngx_rtmp_combined_fmt`.
pub const COMBINED: &str = "$remote_addr [$time_local] $command \"$app\" \"$name\" \"$args\" - \
                            $bytes_received $bytes_sent \"$pageurl\" \"$flashver\" ($session_readable_time)";

/// Log path used when no `access_log` directive is given (`NGX_HTTP_LOG_PATH`).
pub const DEFAULT_PATH: &str = "logs/access.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogVar {
    Connection,
    RemoteAddr,
    App,
    FlashVer,
    SwfUrl,
    TcUrl,
    PageUrl,
    Command,
    Name,
    Args,
    BytesSent,
    BytesReceived,
    TimeLocal,
    Msec,
    SessionTime,
    SessionReadableTime,
}

impl LogVar {
    fn from_name(n: &str) -> Option<Self> {
        Some(match n {
            "connection" => Self::Connection,
            "remote_addr" => Self::RemoteAddr,
            "app" => Self::App,
            "flashver" => Self::FlashVer,
            "swfurl" => Self::SwfUrl,
            "tcurl" => Self::TcUrl,
            "pageurl" => Self::PageUrl,
            "command" => Self::Command,
            "name" => Self::Name,
            "args" => Self::Args,
            "bytes_sent" => Self::BytesSent,
            "bytes_received" => Self::BytesReceived,
            "time_local" => Self::TimeLocal,
            "msec" => Self::Msec,
            "session_time" => Self::SessionTime,
            "session_readable_time" => Self::SessionReadableTime,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogOp {
    Lit(String),
    Var(LogVar),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFormat {
    pub ops: Vec<LogOp>,
}

impl LogFormat {
    /// `ngx_rtmp_log_compile_format`: the arguments are concatenated.
    pub fn compile<S: AsRef<str>>(parts: &[S]) -> Result<Self, String> {
        let mut ops = Vec::new();
        for part in parts {
            let d = part.as_ref().as_bytes();
            let mut i = 0;
            while i < d.len() {
                if d[i] == b'$' {
                    i += 1;
                    let bracket = d.get(i) == Some(&b'{');
                    if bracket {
                        i += 1;
                    }
                    let start = i;
                    while i < d.len() && (d[i].is_ascii_alphanumeric() || d[i] == b'_') {
                        i += 1;
                    }
                    let var = &part.as_ref()[start..i];
                    if bracket {
                        if d.get(i) != Some(&b'}') {
                            return Err(format!("missing closing bracket in \"{var}\""));
                        }
                        i += 1;
                    }
                    if var.is_empty() {
                        return Err(format!("invalid parameter \"{}\"", &part.as_ref()[start.saturating_sub(1)..]));
                    }
                    let v = LogVar::from_name(var).ok_or_else(|| format!("unknown variable \"{var}\""))?;
                    ops.push(LogOp::Var(v));
                } else {
                    let start = i;
                    i += 1;
                    while i < d.len() && d[i] != b'$' {
                        i += 1;
                    }
                    ops.push(LogOp::Lit(part.as_ref()[start..i].to_string()));
                }
            }
        }
        Ok(Self { ops })
    }

    pub fn combined() -> Self {
        Self::compile(&[COMBINED]).expect("built-in format")
    }

    /// Format one log line (without the line feed).
    pub fn render(&self, e: &LogEntry, now: DateTime<Local>) -> String {
        let mut out = String::with_capacity(160);
        for op in &self.ops {
            match op {
                LogOp::Lit(s) => out.push_str(s),
                LogOp::Var(v) => {
                    let _ = match v {
                        LogVar::Connection => write!(out, "{}", e.connection),
                        LogVar::RemoteAddr => write!(out, "{}", e.addr),
                        LogVar::App => write!(out, "{}", e.app),
                        LogVar::FlashVer => write!(out, "{}", e.flashver),
                        LogVar::SwfUrl => write!(out, "{}", e.swf_url),
                        LogVar::TcUrl => write!(out, "{}", e.tc_url),
                        LogVar::PageUrl => write!(out, "{}", e.page_url),
                        LogVar::Command => write!(
                            out,
                            "{}",
                            match (e.play, e.publish) {
                                (false, false) => "NONE",
                                (true, false) => "PLAY",
                                (false, true) => "PUBLISH",
                                (true, true) => "PLAY+PUBLISH",
                            }
                        ),
                        LogVar::Name => write!(out, "{}", e.name),
                        LogVar::Args => write!(out, "{}", e.args),
                        LogVar::BytesSent => write!(out, "{}", e.bytes_sent),
                        LogVar::BytesReceived => write!(out, "{}", e.bytes_received),
                        LogVar::TimeLocal => write!(out, "{}", now.format("%d/%b/%Y:%H:%M:%S %z")),
                        LogVar::Msec => {
                            let t = now.timestamp_millis();
                            write!(out, "{}.{:03}", t.div_euclid(1000), t.rem_euclid(1000))
                        }
                        LogVar::SessionTime => write!(out, "{}", e.session_secs),
                        LogVar::SessionReadableTime => {
                            let v = e.session_secs;
                            let (d, h, m, s) = (v / 86400, v / 3600 % 24, v / 60 % 60, v % 60);
                            if d > 0 {
                                let _ = write!(out, "{d}d ");
                            }
                            if d > 0 || h > 0 {
                                let _ = write!(out, "{h}h ");
                            }
                            if d > 0 || h > 0 || m > 0 {
                                let _ = write!(out, "{m}m ");
                            }
                            write!(out, "{s}s")
                        }
                    };
                }
            }
        }
        out
    }
}

/// Session values available to log formats.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub connection: u64,
    pub addr: IpAddr,
    pub app: String,
    pub flashver: String,
    pub swf_url: String,
    pub tc_url: String,
    pub page_url: String,
    pub play: bool,
    pub publish: bool,
    pub name: String,
    pub args: String,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub session_secs: u64,
}

impl LogEntry {
    pub fn new(connection: u64, addr: IpAddr) -> Self {
        Self {
            connection,
            addr,
            app: String::new(),
            flashver: String::new(),
            swf_url: String::new(),
            tc_url: String::new(),
            page_url: String::new(),
            play: false,
            publish: false,
            name: String::new(),
            args: String::new(),
            bytes_sent: 0,
            bytes_received: 0,
            session_secs: 0,
        }
    }
}

/// An open log file, shared by every `access_log` naming the same path.
pub struct LogFile {
    pub path: PathBuf,
    file: Mutex<std::fs::File>,
}

/// One `access_log` directive, ready to write.
#[derive(Clone)]
pub struct LogTarget {
    pub file: Arc<LogFile>,
    pub format: Arc<LogFormat>,
}

impl std::fmt::Debug for LogTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogTarget").field("path", &self.file.path).finish()
    }
}

/// Write `e` to every target (`ngx_rtmp_log_disconnect`).
pub fn write(targets: &[LogTarget], e: &LogEntry) {
    if targets.is_empty() {
        return;
    }
    let now = DateTime::<Local>::from(SystemTime::now());
    for t in targets {
        let mut line = t.format.render(e, now);
        line.push('\n');
        let mut f = t.file.file.lock().unwrap();
        if let Err(err) = f.write_all(line.as_bytes()) {
            tracing::error!("write to \"{}\" failed: {err}", t.file.path.display());
        }
    }
}

/// Resolve a log path against the prefix (nginx resolves relative paths
/// against its prefix directory).
pub fn resolve_path(prefix: Option<&Path>, path: &str) -> PathBuf {
    let p = Path::new(path);
    match prefix {
        Some(pre) if p.is_relative() => pre.join(p),
        _ => p.to_path_buf(),
    }
}

/// Open every log file named in `config` and attach the resulting targets
/// to the server and application configurations. A missing parent
/// directory is created.
pub fn open_all(config: &mut Config) -> std::io::Result<()> {
    let prefix = config.prefix.clone();
    let formats: HashMap<String, Arc<LogFormat>> =
        config.log_formats.iter().map(|(n, f)| (n.clone(), Arc::new(f.clone()))).collect();
    let mut files: HashMap<PathBuf, Arc<LogFile>> = HashMap::new();
    let mut targets_for = |conf: &AccessLogConf| -> std::io::Result<Vec<LogTarget>> {
        let defs = match conf {
            AccessLogConf::Off => return Ok(Vec::new()),
            AccessLogConf::Logs(defs) => defs.clone(),
            // No directive at any level: nginx logs to logs/access.log. Only
            // done for configurations loaded with a prefix (the binary), so
            // embedded/test configurations don't write files implicitly.
            AccessLogConf::Unset if prefix.is_some() => {
                vec![(DEFAULT_PATH.to_string(), "combined".to_string())]
            }
            AccessLogConf::Unset => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for (path, fmt) in defs {
            let path = resolve_path(prefix.as_deref(), &path);
            let file = match files.get(&path) {
                Some(f) => f.clone(),
                None => {
                    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                        std::fs::create_dir_all(dir).map_err(|e| annotate(&path, e))?;
                    }
                    let f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .map_err(|e| annotate(&path, e))?;
                    let f = Arc::new(LogFile { path: path.clone(), file: Mutex::new(f) });
                    files.insert(path, f.clone());
                    f
                }
            };
            let format = formats.get(&fmt).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("unknown log format \"{fmt}\""))
            })?;
            out.push(LogTarget { file, format });
        }
        Ok(out)
    };
    for srv in &mut config.servers {
        srv.access_logs = targets_for(&srv.access_log)?;
        for app in &mut srv.applications {
            app.access_logs = targets_for(&app.access_log)?;
        }
    }
    Ok(())
}

fn annotate(path: &Path, e: std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("cannot open access log \"{}\": {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn entry() -> LogEntry {
        LogEntry {
            app: "live".into(),
            flashver: "FMLE/3.0".into(),
            publish: true,
            name: "cam".into(),
            args: "key=1".into(),
            bytes_sent: 3_000,
            bytes_received: 1_234_567,
            session_secs: 3_725,
            ..LogEntry::new(7, "10.0.0.5".parse().unwrap())
        }
    }

    #[test]
    fn combined_line() {
        let now = Local.with_ymd_and_hms(2026, 11, 14, 23, 5, 9).unwrap();
        let line = LogFormat::combined().render(&entry(), now);
        let tz = now.format("%z");
        assert_eq!(
            line,
            format!(
                "10.0.0.5 [14/Nov/2026:23:05:09 {tz}] PUBLISH \"live\" \"cam\" \"key=1\" - 1234567 3000 \"\" \"FMLE/3.0\" (1h 2m 5s)"
            )
        );
    }

    #[test]
    fn custom_format_and_errors() {
        let f = LogFormat::compile(&["$connection ${app}x", " $session_time"]).unwrap();
        let now = Local::now();
        assert_eq!(f.render(&entry(), now), "7 livex 3725");
        assert!(LogFormat::compile(&["$nope"]).unwrap_err().contains("unknown variable"));
        assert!(LogFormat::compile(&["${app"]).unwrap_err().contains("closing bracket"));
        assert!(LogFormat::compile(&["a $"]).is_err());
    }

    #[test]
    fn readable_time() {
        let f = LogFormat::compile(&["$session_readable_time"]).unwrap();
        let mut e = entry();
        let now = Local::now();
        for (secs, want) in [(5, "5s"), (65, "1m 5s"), (3600, "1h 0m 0s"), (90061, "1d 1h 1m 1s")] {
            e.session_secs = secs;
            assert_eq!(f.render(&e, now), want);
        }
    }
}
