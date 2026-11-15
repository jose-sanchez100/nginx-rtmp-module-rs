//! `ngx_rtmp_exec_module.c`: run external programs (typically ffmpeg) on
//! stream events.
//!
//! * `exec` / `exec_push`: managed, one per publisher; started on publish,
//!   killed when the publisher leaves, respawned if it exits on its own.
//! * `exec_pull`: managed, one per stream; started by the first player,
//!   killed when the last one leaves.
//! * `exec_static`: managed, started with the server and kept running.
//! * `exec_publish`, `exec_publish_done`, `exec_play`, `exec_play_done`:
//!   one-shot commands, never killed.
//!
//! The C module is compiled out on Windows; this one runs everywhere. On
//! Windows each managed child is placed in its own Job Object, so killing it
//! (or the server dying) also kills anything it started (`cmd /c` wrappers),
//! and children never open a console window.

use crate::access_log::resolve_path;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tracing::{Instrument, debug, error, info, warn};

/// One `exec*` directive (`ngx_rtmp_exec_conf_t`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecConf {
    pub cmd: String,
    pub args: Vec<String>,
    /// `name=` filters (with `exec_options on`): only run for these streams.
    pub names: Vec<String>,
}

impl ExecConf {
    fn matches(&self, name: &str) -> bool {
        self.names.is_empty() || self.names.iter().any(|n| n == name)
    }
}

/// Application-level settings (`ngx_rtmp_exec_app_conf_t`).
#[derive(Debug, Clone)]
pub struct AppExec {
    pub push: Vec<Arc<ExecConf>>,
    pub pull: Vec<Arc<ExecConf>>,
    pub publish: Vec<Arc<ExecConf>>,
    pub publish_done: Vec<Arc<ExecConf>>,
    pub play: Vec<Arc<ExecConf>>,
    pub play_done: Vec<Arc<ExecConf>>,
    /// Parsed; fires once `record` is ported.
    pub record_done: Vec<Arc<ExecConf>>,
    /// `respawn`: restart managed children that exit on their own.
    pub respawn: bool,
    /// `exec_options`: accept `name=` filters in exec arguments.
    pub options: bool,
}

impl Default for AppExec {
    fn default() -> Self {
        Self {
            push: Vec::new(),
            pull: Vec::new(),
            publish: Vec::new(),
            publish_done: Vec::new(),
            play: Vec::new(),
            play_done: Vec::new(),
            record_done: Vec::new(),
            respawn: true,
            options: false,
        }
    }
}

impl AppExec {
    pub fn is_active(&self) -> bool {
        [&self.push, &self.pull, &self.publish, &self.publish_done, &self.play, &self.play_done, &self.record_done]
            .iter()
            .any(|v| !v.is_empty())
    }
}

/// `exec_kill_signal`. Windows always terminates the process tree.
#[derive(Debug, Clone, PartialEq)]
pub enum KillSignal {
    Kill,
    /// Signal name (`TERM`) or number, passed to `kill -s`.
    Other(String),
}

const SIGNALS: [&str; 19] = [
    "HUP", "INT", "QUIT", "ILL", "ABRT", "FPE", "KILL", "SEGV", "PIPE", "ALRM", "TERM", "USR1", "USR2", "CHLD",
    "CONT", "STOP", "TSTP", "TTIN", "TTOU",
];

impl KillSignal {
    pub fn parse(v: &str) -> Option<Self> {
        if let Ok(n) = v.parse::<u32>() {
            return Some(if n == 9 { Self::Kill } else { Self::Other(n.to_string()) });
        }
        let up = v.to_ascii_uppercase();
        match up.as_str() {
            "KILL" => Some(Self::Kill),
            s if SIGNALS.contains(&s) => Some(Self::Other(up)),
            _ => None,
        }
    }
}

/// Main-level settings (`ngx_rtmp_exec_main_conf_t`).
#[derive(Debug, Clone)]
pub struct MainExec {
    pub statics: Vec<Arc<ExecConf>>,
    pub respawn_timeout: Duration,
    pub kill_signal: KillSignal,
    /// Relative program paths, redirect files and the children's working
    /// directory are resolved against the prefix.
    pub prefix: Option<PathBuf>,
}

impl Default for MainExec {
    fn default() -> Self {
        Self { statics: Vec::new(), respawn_timeout: Duration::from_secs(5), kill_signal: KillSignal::Kill, prefix: None }
    }
}

impl MainExec {
    /// Start the `exec_static` children; they run until the returned
    /// handles are dropped.
    pub fn start_statics(&self) -> Vec<Managed> {
        self.statics
            .iter()
            .map(|c| {
                let span = tracing::info_span!("exec_static", cmd = %c.cmd);
                let _g = span.enter();
                // Static commands are not evaluated (no session).
                spawn_managed(prepare(c, None, self.prefix.as_deref()), Some(self.respawn_timeout), self.kill_signal.clone())
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Argument evaluation (`ngx_rtmp_eval.c`)
// ---------------------------------------------------------------------------

pub type Vars = Vec<(&'static str, String)>;

/// Expand `$var` / `${var}`; unknown variables expand to nothing. As in
/// nginx, a backslash makes the next character literal (`\$`), except on
/// Windows, where backslashes are path separators and kept as they are
/// (`C:\rec\$name.flv`).
pub fn eval(s: &str, vars: &Vars) -> String {
    let lookup = |n: &str| vars.iter().find(|(k, _)| *k == n).map_or("", |(_, v)| v.as_str());
    let specials: &[char] = if cfg!(windows) { &['$'] } else { &['$', '\\'] };
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(specials) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        if rest.as_bytes()[pos] == b'\\' {
            let mut cs = after.chars();
            if let Some(c) = cs.next() {
                out.push(c);
            }
            rest = cs.as_str();
            continue;
        }
        if let Some(inner) = after.strip_prefix('{') {
            match inner.find('}') {
                Some(e) => {
                    out.push_str(lookup(&inner[..e]));
                    rest = &inner[e + 1..];
                }
                None => rest = "",
            }
        } else {
            let n = after.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(after.len());
            out.push_str(lookup(&after[..n]));
            rest = &after[n..];
        }
    }
    out.push_str(rest);
    out
}

/// Standard stream redirection written as an argument
/// (`ngx_rtmp_eval_streams`): `>file`, `>>file`, `2>>file`, `<file`, `2>&1`.
#[derive(Debug, Clone, PartialEq)]
enum Redirect {
    File { fd: usize, path: PathBuf, append: bool, read: bool },
    Dup { fd: usize, src: usize },
}

fn parse_redirect(arg: &str, prefix: Option<&Path>) -> Option<Result<Redirect, String>> {
    let digits = arg.find(|c: char| !c.is_ascii_digit()).unwrap_or(arg.len());
    let (num, rest) = arg.split_at(digits);
    let (read, rest) = match rest.as_bytes().first()? {
        b'>' => (false, &rest[1..]),
        b'<' => (true, &rest[1..]),
        _ => return None,
    };
    let fd = if num.is_empty() { usize::from(!read) } else { num.parse().unwrap_or(usize::MAX) };
    if fd > 2 {
        return Some(Err(format!("unsupported redirection \"{arg}\" (only 0, 1 and 2)")));
    }
    let (append, rest) = match rest.strip_prefix('>') {
        Some(r) if !read => (true, r),
        _ => (false, rest),
    };
    if let Some(src) = rest.strip_prefix('&') {
        return Some(match src.parse::<usize>() {
            Ok(src) if src <= 2 => Ok(Redirect::Dup { fd, src }),
            _ => Err(format!("unsupported redirection \"{arg}\"")),
        });
    }
    if rest.is_empty() {
        return Some(Err(format!("missing file name in \"{arg}\"")));
    }
    Some(Ok(Redirect::File { fd, path: resolve_path(prefix, rest), append, read }))
}

/// A command ready to start (arguments evaluated once).
#[derive(Debug, Clone)]
struct Prepared {
    /// As written, for logs.
    cmd: String,
    program: PathBuf,
    args: Vec<String>,
    redirects: Vec<Redirect>,
    cwd: Option<PathBuf>,
}

fn prepare(conf: &ExecConf, vars: Option<&Vars>, prefix: Option<&Path>) -> Prepared {
    let mut args = Vec::new();
    let mut redirects = Vec::new();
    for a in &conf.args {
        let v = vars.map_or_else(|| a.clone(), |vars| eval(a, vars));
        match parse_redirect(&v, prefix) {
            None => args.push(v),
            Some(Ok(r)) => redirects.push(r),
            Some(Err(e)) => warn!("exec: {e}, ignored"),
        }
    }
    // A bare name ("ffmpeg") is looked up in PATH; a relative path
    // ("scripts/x.bat") is relative to the prefix.
    let has_sep = conf.cmd.contains('/') || (cfg!(windows) && conf.cmd.contains('\\'));
    let program = if has_sep { resolve_path(prefix, &conf.cmd) } else { PathBuf::from(&conf.cmd) };
    Prepared { cmd: conf.cmd.clone(), program, args, redirects, cwd: prefix.map(Path::to_path_buf) }
}

impl Prepared {
    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        let mut fds: [Option<std::fs::File>; 3] = [None, None, None];
        for r in &self.redirects {
            match r {
                Redirect::File { fd, path, append, read } => {
                    let f = if *read {
                        std::fs::File::open(path)
                    } else {
                        if let Some(dir) = path.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .append(*append)
                            .truncate(!*append)
                            .open(path)
                    };
                    fds[*fd] = match f {
                        Ok(f) => Some(f),
                        Err(e) => {
                            warn!("exec: cannot open \"{}\": {e}", path.display());
                            None
                        }
                    };
                }
                Redirect::Dup { fd, src } => fds[*fd] = fds[*src].as_ref().and_then(|f| f.try_clone().ok()),
            }
        }
        let [i, o, e] = fds;
        let stdio = |f: Option<std::fs::File>| f.map_or_else(Stdio::null, Stdio::from);
        c.stdin(stdio(i)).stdout(stdio(o)).stderr(stdio(e));
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            c.creation_flags(CREATE_NO_WINDOW);
        }
        c
    }

    fn start(&self, managed: bool) -> std::io::Result<Proc> {
        let mut c = self.command();
        c.kill_on_drop(managed);
        let child = c.spawn()?;
        #[cfg(windows)]
        let job = if managed { job::Job::adopt(&child) } else { None };
        Ok(Proc {
            child,
            #[cfg(windows)]
            _job: job,
        })
    }
}

struct Proc {
    child: Child,
    /// Dropping it kills the whole process tree.
    #[cfg(windows)]
    _job: Option<job::Job>,
}

impl Proc {
    async fn terminate(mut self, kill: &KillSignal) {
        #[cfg(windows)]
        {
            let _ = kill;
            drop(self._job.take());
            let _ = self.child.start_kill();
        }
        #[cfg(unix)]
        match (kill, self.child.id()) {
            (KillSignal::Other(sig), Some(pid)) => {
                let r = Command::new("kill").args(["-s", sig, &pid.to_string()]).status().await;
                if !matches!(r, Ok(s) if s.success()) {
                    info!("exec: kill -s {sig} {pid} failed: {r:?}");
                }
            }
            _ => {
                let _ = self.child.start_kill();
            }
        }
        // Reap it without waiting here.
        tokio::spawn(async move {
            let _ = self.child.wait().await;
        });
    }
}

// ---------------------------------------------------------------------------
// Managed and unmanaged children
// ---------------------------------------------------------------------------

/// A supervised child: respawned after `respawn_timeout` when it exits,
/// killed (with `exec_kill_signal`) when this handle is dropped.
pub struct Managed {
    _stop: oneshot::Sender<()>,
}

fn spawn_managed(p: Prepared, respawn: Option<Duration>, kill: KillSignal) -> Managed {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(supervise(p, respawn, kill, rx).in_current_span());
    Managed { _stop: tx }
}

async fn supervise(p: Prepared, respawn: Option<Duration>, kill: KillSignal, mut stop: oneshot::Receiver<()>) {
    loop {
        info!("exec: starting managed child '{}'", p.cmd);
        let mut delay = respawn;
        match p.start(true) {
            Ok(mut proc) => {
                let pid = proc.child.id().unwrap_or(0);
                debug!("exec: child '{}' started pid={pid} args={:?}", p.cmd, p.args);
                let exited = tokio::select! {
                    st = proc.child.wait() => Some(st),
                    _ = &mut stop => None,
                };
                match exited {
                    Some(st) => {
                        let st = st.map_or_else(|e| e.to_string(), |s| s.to_string());
                        let what = if respawn.is_some() { "respawning" } else { "ignoring" };
                        info!("exec: child {pid} exited ({st}); {what}");
                    }
                    None => {
                        info!("exec: terminating child {pid}");
                        proc.terminate(&kill).await;
                        return;
                    }
                }
            }
            Err(e) => {
                error!("exec: cannot start '{}': {e}", p.program.display());
                // Don't spin on a missing program with `respawn_timeout 0`.
                delay = delay.map(|d| d.max(Duration::from_secs(1)));
            }
        }
        let Some(d) = delay else { return };
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            _ = &mut stop => return,
        }
    }
}

/// Fire-and-forget command (`exec_publish` and friends).
fn run_unmanaged(p: Prepared) {
    info!("exec: starting unmanaged child '{}'", p.cmd);
    match p.start(false) {
        Ok(mut proc) => {
            let cmd = p.cmd;
            tokio::spawn(
                async move {
                    let pid = proc.child.id().unwrap_or(0);
                    match proc.child.wait().await {
                        Ok(st) => debug!("exec: child '{cmd}' pid={pid} exited ({st})"),
                        Err(e) => debug!("exec: child '{cmd}' pid={pid}: {e}"),
                    }
                }
                .in_current_span(),
            );
        }
        Err(e) => error!("exec: cannot start '{}': {e}", p.program.display()),
    }
}

/// `exec_record_done` for one closed recording (`ngx_rtmp_exec_record_done`):
/// adds `$recorder`, `$path`, `$filename`, `$basename` and `$dirname` to
/// the session variables.
#[allow(clippy::too_many_arguments)]
pub fn record_done(
    main: &MainExec,
    list: &[Arc<ExecConf>],
    mut vars: Vars,
    name: &str,
    args: &str,
    recorder: &str,
    path: &str,
) {
    let (dirname, filename) = match path.rfind(['/', '\\']) {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let basename = filename.rfind('.').map_or(filename, |i| &filename[..i]);
    vars.extend([
        ("name", name.to_string()),
        ("args", args.to_string()),
        ("recorder", recorder.to_string()),
        ("path", path.to_string()),
        ("filename", filename.to_string()),
        ("basename", basename.to_string()),
        ("dirname", dirname.to_string()),
    ]);
    for c in list.iter().filter(|c| c.matches(name)) {
        run_unmanaged(prepare(c, Some(&vars), main.prefix.as_deref()));
    }
}

// ---------------------------------------------------------------------------
// Per-stream and per-session state
// ---------------------------------------------------------------------------

/// `exec_pull` children of one stream, shared by its players.
pub struct PullGroup {
    _procs: Vec<Managed>,
}

/// Running `exec_pull` groups of a `server {}`, by (application, stream).
#[derive(Default)]
pub struct PullTable(Mutex<HashMap<(String, String), Weak<PullGroup>>>);

/// `ngx_rtmp_exec_ctx_t`.
#[derive(Default)]
pub struct SessionExec {
    publishing: bool,
    playing: bool,
    name: String,
    args: String,
    push: Vec<Managed>,
    pull: Option<Arc<PullGroup>>,
}

impl SessionExec {
    /// After a successful publish: `exec_publish` and the `exec_push`
    /// children. `vars` holds the session variables.
    pub fn publish(&mut self, main: &MainExec, app: &AppExec, mut vars: Vars, name: &str, args: &str) {
        if !app.is_active() {
            return;
        }
        self.publishing = true;
        self.name = name.to_string();
        self.args = args.to_string();
        vars.push(("name", name.to_string()));
        vars.push(("args", args.to_string()));
        let prefix = main.prefix.as_deref();
        for c in app.publish.iter().filter(|c| c.matches(name)) {
            run_unmanaged(prepare(c, Some(&vars), prefix));
        }
        let respawn = app.respawn.then_some(main.respawn_timeout);
        self.push = app
            .push
            .iter()
            .filter(|c| c.matches(name))
            .map(|c| spawn_managed(prepare(c, Some(&vars), prefix), respawn, main.kill_signal.clone()))
            .collect();
    }

    /// Before joining the stream as a player: `exec_play`, and the
    /// `exec_pull` children if this is the stream's first player.
    #[allow(clippy::too_many_arguments)]
    pub fn play(
        &mut self,
        main: &MainExec,
        app: &AppExec,
        pulls: &PullTable,
        app_name: &str,
        mut vars: Vars,
        name: &str,
        args: &str,
    ) {
        if !app.is_active() {
            return;
        }
        self.playing = true;
        self.name = name.to_string();
        self.args = args.to_string();
        vars.push(("name", name.to_string()));
        vars.push(("args", args.to_string()));
        let prefix = main.prefix.as_deref();
        for c in app.play.iter().filter(|c| c.matches(name)) {
            run_unmanaged(prepare(c, Some(&vars), prefix));
        }
        if app.pull.is_empty() || self.pull.is_some() {
            return;
        }
        let key = (app_name.to_string(), name.to_string());
        let mut map = pulls.0.lock().unwrap();
        if let Some(g) = map.get(&key).and_then(Weak::upgrade) {
            self.pull = Some(g);
            return;
        }
        map.retain(|_, w| w.strong_count() > 0);
        // exec_pull only knows the stream, not the session.
        let pvars: Vars = vec![("name", name.to_string()), ("app", app_name.to_string())];
        let respawn = app.respawn.then_some(main.respawn_timeout);
        let g = Arc::new(PullGroup {
            _procs: app
                .pull
                .iter()
                .filter(|c| c.matches(name))
                .map(|c| spawn_managed(prepare(c, Some(&pvars), prefix), respawn, main.kill_signal.clone()))
                .collect(),
        });
        map.insert(key, Arc::downgrade(&g));
        self.pull = Some(g);
    }

    /// `ngx_rtmp_exec_close_stream`: `exec_*_done`, then stop this
    /// session's children.
    pub fn close(&mut self, main: &MainExec, app: &AppExec, mut vars: Vars) {
        if self.publishing || self.playing {
            vars.push(("name", std::mem::take(&mut self.name)));
            vars.push(("args", std::mem::take(&mut self.args)));
            let name = vars[vars.len() - 2].1.clone();
            let prefix = main.prefix.as_deref();
            let run = |list: &[Arc<ExecConf>]| {
                for c in list.iter().filter(|c| c.matches(&name)) {
                    run_unmanaged(prepare(c, Some(&vars), prefix));
                }
            };
            if self.publishing {
                run(&app.publish_done);
            }
            if self.playing {
                run(&app.play_done);
            }
        }
        self.publishing = false;
        self.playing = false;
        self.push.clear();
        self.pull = None;
    }
}

#[cfg(windows)]
mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    /// A Job Object that kills its processes when closed.
    pub struct Job(HANDLE);

    // SAFETY: a kernel handle, usable from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn adopt(child: &tokio::process::Child) -> Option<Job> {
            let process = child.raw_handle()?;
            // SAFETY: plain Win32 calls on handles we own; `info` outlives
            // the call that reads it.
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let job = Job(h);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 || AssignProcessToJobObject(job.0, process as HANDLE) == 0 {
                    tracing::debug!("exec: job object: {}", std::io::Error::last_os_error());
                    return None;
                }
                Some(job)
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is ours and closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        vec![("name", "cam".into()), ("app", "live".into()), ("addr", "10.0.0.5".into())]
    }

    #[test]
    fn evaluation() {
        assert_eq!(eval("rtmp://localhost/$app/$name", &vars()), "rtmp://localhost/live/cam");
        assert_eq!(eval("${name}_hd.flv", &vars()), "cam_hd.flv");
        assert_eq!(eval("$namex-$bogus-", &vars()), "--");
        if cfg!(windows) {
            assert_eq!(eval(r"C:\rec\$name.flv", &vars()), r"C:\rec\cam.flv");
        } else {
            assert_eq!(eval(r"cost \$5 a\b", &vars()), "cost $5 ab");
        }
        assert_eq!(eval("a$1", &vars()), "a1");
        assert_eq!(eval("${name", &vars()), "");
        assert_eq!(eval("$addr", &vars()), "10.0.0.5");
    }

    #[test]
    fn redirects() {
        let p = Some(Path::new("/srv"));
        let r = |a| parse_redirect(a, p);
        assert_eq!(r("-i"), None);
        assert_eq!(r("rtmp://x"), None);
        assert_eq!(r("123"), None);
        assert_eq!(
            r("2>>logs/ff.log"),
            Some(Ok(Redirect::File { fd: 2, path: Path::new("/srv").join("logs/ff.log"), append: true, read: false }))
        );
        assert_eq!(
            r(">/tmp/o"),
            Some(Ok(Redirect::File { fd: 1, path: "/tmp/o".into(), append: false, read: false }))
        );
        assert_eq!(r("<in"), Some(Ok(Redirect::File { fd: 0, path: Path::new("/srv").join("in"), append: false, read: true })));
        assert_eq!(r("2>&1"), Some(Ok(Redirect::Dup { fd: 2, src: 1 })));
        assert!(matches!(r("3>x"), Some(Err(_))));
        assert!(matches!(r(">"), Some(Err(_))));
    }

    #[test]
    fn kill_signals() {
        assert_eq!(KillSignal::parse("kill"), Some(KillSignal::Kill));
        assert_eq!(KillSignal::parse("9"), Some(KillSignal::Kill));
        assert_eq!(KillSignal::parse("term"), Some(KillSignal::Other("TERM".into())));
        assert_eq!(KillSignal::parse("15"), Some(KillSignal::Other("15".into())));
        assert_eq!(KillSignal::parse("SIGTERM"), None);
    }

    #[test]
    fn name_filter() {
        let c = ExecConf { cmd: "x".into(), args: vec![], names: vec!["a".into(), "b".into()] };
        assert!(c.matches("b") && !c.matches("c"));
    }
}
