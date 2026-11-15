//! Process plumbing for the binary, after nginx: the error log file, the pid
//! file, `-s stop|quit`, and detaching from the console when started from
//! Explorer so no window stays open.

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use tracing::Level;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::{self, format::Writer, time::FormatTime, MakeWriter};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{reload, EnvFilter, Registry};

/// Our crate's target; the lib and the binary are both `rtmp_server`.
const TARGET: &str = "rtmp_server";

// ---------------------------------------------------------------------
// Console
// ---------------------------------------------------------------------

/// Release the console if this process is the only one using it, which is
/// what happens when the executable is double-clicked (Windows creates a
/// console just for it). Started from a terminal, the console is kept.
/// Returns whether there is still a console to write to.
#[cfg(windows)]
pub fn detach_console() -> bool {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut list = [0u32; 2];
    // SAFETY: the buffer holds 2 entries, as passed.
    match unsafe { GetConsoleProcessList(list.as_mut_ptr(), 2) } {
        0 => false,
        1 => {
            // SAFETY: no arguments; failure leaves the console in place.
            unsafe { FreeConsole() };
            false
        }
        _ => true,
    }
}

#[cfg(not(windows))]
pub fn detach_console() -> bool {
    true
}

// ---------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------

static ERROR_LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn error_log() -> &'static Mutex<Option<File>> {
    ERROR_LOG.get_or_init(|| Mutex::new(None))
}

/// Writes formatted events to the error log file, if one is open.
pub struct ErrorLogWriter;

impl Write for ErrorLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(f) = error_log().lock().unwrap().as_mut() {
            // A failing log must not take the server down.
            let _ = f.write_all(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for ErrorLogWriter {
    type Writer = ErrorLogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ErrorLogWriter
    }
}

/// nginx's error log time format.
struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y/%m/%d %H:%M:%S"))
    }
}

/// Changes the error log file and level once the configuration is known.
pub struct LogHandle(reload::Handle<Targets, Registry>);

/// Console output (when there is a console) is filtered by `RUST_LOG`,
/// default `info`; the error log file by its configured level.
pub fn init_logging(console: bool) -> LogHandle {
    let (file_filter, handle) = reload::Layer::new(Targets::new());
    let file = fmt::layer()
        .with_ansi(false)
        .with_timer(LocalTime)
        .with_target(false)
        .with_writer(ErrorLogWriter)
        .with_filter(file_filter);
    let console = console.then(|| {
        fmt::layer().with_writer(io::stderr).with_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| format!("{TARGET}=info").into()),
        )
    });
    tracing_subscriber::registry().with(file).with(console).init();
    LogHandle(handle)
}

impl LogHandle {
    /// Open (append) `path` as the error log at the given nginx level.
    pub fn open(&self, path: &Path, level: &str) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| io::Error::new(e.kind(), format!("open \"{}\" failed: {e}", path.display())))?;
        *error_log().lock().unwrap() = Some(f);
        let level = match level {
            "debug" => Level::DEBUG,
            "info" | "notice" => Level::INFO,
            "warn" => Level::WARN,
            _ => Level::ERROR,
        };
        let _ = self.0.reload(Targets::new().with_target(TARGET, LevelFilter::from_level(level)));
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Pid file and signals
// ---------------------------------------------------------------------

pub fn write_pid(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{}\n", std::process::id()))
        .map_err(|e| io::Error::new(e.kind(), format!("write \"{}\" failed: {e}", path.display())))
}

pub fn read_pid(path: &Path) -> Result<u32, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("open \"{}\" failed: {e}", path.display()))?;
    s.trim()
        .parse()
        .map_err(|_| format!("invalid PID number \"{}\" in \"{}\"", s.trim(), path.display()))
}

/// Signals accepted by `-s`.
pub const SIGNALS: [&str; 2] = ["stop", "quit"];

pub type SignalFuture = Pin<Box<dyn Future<Output = &'static str> + Send>>;

#[cfg(windows)]
fn event_name(sig: &str, pid: u32) -> Vec<u16> {
    format!("rtmp-server_{sig}_{pid}").encode_utf16().chain([0]).collect()
}

/// `-s stop|quit`: signal the running server, like `nginx -s`. On Windows
/// this sets a named event the server waits on.
#[cfg(windows)]
pub fn send_signal(sig: &str, pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    let name = event_name(sig, pid);
    // SAFETY: `name` is NUL-terminated UTF-16.
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if h.is_null() {
        return Err(format!("no running server with PID {pid}: {}", io::Error::last_os_error()));
    }
    // SAFETY: `h` is a valid event handle opened above.
    let ok = unsafe { SetEvent(h) } != 0;
    let err = io::Error::last_os_error();
    // SAFETY: closing the handle we own.
    unsafe { CloseHandle(h) };
    if ok { Ok(()) } else { Err(format!("SetEvent failed: {err}")) }
}

#[cfg(unix)]
pub fn send_signal(sig: &str, pid: u32) -> Result<(), String> {
    let s = if sig == "quit" { "-QUIT" } else { "-TERM" };
    match std::process::Command::new("kill").args([s, &pid.to_string()]).status() {
        Ok(st) if st.success() => Ok(()),
        Ok(st) => Err(format!("kill {s} {pid} failed: {st}")),
        Err(e) => Err(format!("kill {s} {pid} failed: {e}")),
    }
}

/// Start listening for `-s` signals sent to this process. The future
/// resolves with the signal name.
#[cfg(windows)]
pub fn listen_signals() -> io::Result<SignalFuture> {
    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForMultipleObjects};
    let pid = std::process::id();
    let mut handles = Vec::new();
    for sig in SIGNALS {
        let name = event_name(sig, pid);
        // SAFETY: default security, auto-reset, initially unset; `name` is NUL-terminated.
        let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Handles are plain kernel object ids; pass them as integers.
        handles.push(h as usize);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let hs: Vec<HANDLE> = handles.iter().map(|&h| h as HANDLE).collect();
        // SAFETY: the handles stay open for the life of the process.
        let r = unsafe { WaitForMultipleObjects(hs.len() as u32, hs.as_ptr(), 0, INFINITE) };
        if let Some(&sig) = SIGNALS.get(r.wrapping_sub(WAIT_OBJECT_0) as usize) {
            let _ = tx.send(sig);
        }
    });
    Ok(Box::pin(async move {
        match rx.await {
            Ok(sig) => sig,
            Err(_) => std::future::pending().await,
        }
    }))
}

#[cfg(unix)]
pub fn listen_signals() -> io::Result<SignalFuture> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut quit = signal(SignalKind::quit())?;
    Ok(Box::pin(async move {
        tokio::select! {
            _ = term.recv() => "stop",
            _ = quit.recv() => "quit",
        }
    }))
}
