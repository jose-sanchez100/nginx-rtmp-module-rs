mod daemon;
#[cfg(windows)]
mod editor_window;
#[cfg(windows)]
mod tray;

use nginx_rtmp::access_log::resolve_path;
use nginx_rtmp::config::Config;
use nginx_rtmp::editor;
use nginx_rtmp::server::{Reloader, Server};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

/// Requests from the tray icon.
#[derive(Clone, Copy)]
pub enum TrayCmd {
    Stop,
    Reload,
    Configure,
}

const USAGE: &str = "usage: nginx [-p prefix] [-c nginx.conf] [-t] [-e] [-s stop|quit|reload]

  -p DIR    prefix: relative paths in the configuration (logs, pid) are
            resolved against it; default: the directory of the executable
  -c FILE   configuration file (nginx syntax, rtmp {} block);
            default: PREFIX/conf/nginx.conf, or conf/nginx.conf in the
            working directory when -p is not given and that file is missing
  -t        test configuration and exit
  -e        edit the configuration in a window; saving reloads the
            running server (the tray icon's Configure does the same)
  -s SIGNAL send a signal to the running server: stop, quit, reload

Errors go to the error log (error_log, default PREFIX/logs/error.log).
Console output, when started from a terminal, is controlled by RUST_LOG
(e.g. RUST_LOG=nginx_rtmp=debug).";

const DEFAULT_ERROR_LOG: &str = "logs/error.log";
const DEFAULT_PID: &str = "logs/nginx.pid";

#[tokio::main]
async fn main() -> ExitCode {
    let mut conf_arg: Option<PathBuf> = None;
    let mut prefix_arg: Option<PathBuf> = None;
    let mut test_only = false;
    let mut edit = false;
    let mut signal: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "-p" | "-s" => match (a.as_str(), args.next()) {
                ("-c", Some(v)) => conf_arg = Some(v.into()),
                ("-p", Some(v)) => prefix_arg = Some(v.into()),
                ("-s", Some(v)) if daemon::SIGNALS.contains(&v.as_str()) => signal = Some(v),
                ("-s", Some(v)) => {
                    eprintln!("nginx: invalid option: \"-s {v}\"
{USAGE}");
                    return ExitCode::from(2);
                }
                _ => {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "-t" => test_only = true,
            "-e" => edit = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument {other}
{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let serve = !test_only && !edit && signal.is_none();

    // Started by double-click: drop the console, like nginx. Everything
    // then goes to the error log.
    let console = !serve || daemon::detach_console();
    let log = daemon::init_logging(console);
    let fail = |msg: String| {
        if serve {
            tracing::error!("{msg}");
        } else {
            eprintln!("nginx: {msg}");
        }
        ExitCode::FAILURE
    };

    let (prefix, conf_path) = locate(prefix_arg, conf_arg);
    if serve {
        // Until the configuration is read, use the default error log (so
        // configuration errors are recorded somewhere).
        if let Err(e) = log.open(&prefix.join(DEFAULT_ERROR_LOG), "error") {
            tracing::warn!("{e}");
        }
    }
    if edit {
        return edit_standalone(prefix, conf_path).await;
    }
    let mut config = match Config::load(&conf_path) {
        Ok(c) => c,
        Err(e) => return fail(e.to_string()),
    };
    config.prefix = Some(prefix.clone());
    for w in &config.warnings {
        tracing::warn!("{w}");
    }
    let pid_path = resolve_path(Some(&prefix), config.pid.as_deref().unwrap_or(DEFAULT_PID));

    if let Some(sig) = signal {
        let pid = match daemon::read_pid(&pid_path) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        return match daemon::send_signal(&sig, pid) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        };
    }
    if test_only {
        if let Err(e) = nginx_rtmp::access_log::open_all(&mut config) {
            return fail(e.to_string());
        }
        println!("nginx: configuration file {} test is successful", conf_path.display());
        return ExitCode::SUCCESS;
    }

    if let Some(el) = &config.error_log
        && let Err(e) = log.open(&resolve_path(Some(&prefix), &el.path), &el.level)
    {
        return fail(e.to_string());
    }
    tracing::info!("configuration {}, prefix {}", conf_path.display(), prefix.display());
    let server = match Server::bind(config).await {
        Ok(s) => s,
        Err(e) => return fail(e.to_string()),
    };
    if let Err(e) = daemon::write_pid(&pid_path) {
        return fail(e.to_string());
    }
    let mut signals = match daemon::listen_signals() {
        Ok(s) => s,
        Err(e) => return fail(format!("signals: {e}")),
    };
    let log = Arc::new(log);
    let reload: editor::Apply = {
        let (reloader, conf_path, prefix, log) = (server.reloader(), conf_path.clone(), prefix.clone(), log.clone());
        Arc::new(move || {
            let (reloader, conf_path, prefix, log) = (reloader.clone(), conf_path.clone(), prefix.clone(), log.clone());
            Box::pin(async move { reload_config(&reloader, &conf_path, &prefix, &log).await })
        })
    };
    let (tray_tx, mut tray_rx) = tokio::sync::mpsc::unbounded_channel::<TrayCmd>();
    #[cfg(windows)]
    let toasts = Arc::new(std::sync::Mutex::new(Vec::new()));
    #[cfg(windows)]
    let tray = tray::start(tray::TrayOptions {
        registry: server.registry.clone(),
        listen: server.listen_info_shared(),
        conf_path: conf_path.clone(),
        logs_dir: prefix.join("logs"),
        commands: tray_tx,
        toasts: toasts.clone(),
    });
    #[cfg(windows)]
    if tray.is_none() {
        tracing::warn!("could not create the tray icon");
    }
    #[cfg(not(windows))]
    drop(tray_tx);
    let mut editor: Option<editor::Running> = None;
    // A task of its own: reloads are applied by it while this loop waits
    // for their outcome.
    let mut run = tokio::spawn(server.run());
    loop {
        tokio::select! {
            _ = &mut run => break,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                break;
            }
            Some(sig) = signals.recv() => {
                if sig != "reload" {
                    tracing::info!("signal {sig} received, exiting");
                    break;
                }
                tracing::info!("signal reload received");
                // failures are in the error log already
                let _ = reload().await;
            }
            Some(cmd) = tray_rx.recv() => {
                let toast = match cmd {
                    TrayCmd::Stop => {
                        tracing::info!("stopped from the tray icon");
                        break;
                    }
                    TrayCmd::Reload => match reload().await {
                        Ok(m) => ("Configuration reloaded".to_string(), m),
                        Err(e) => ("Reload failed".to_string(), e),
                    },
                    TrayCmd::Configure => match open_editor(&mut editor, &conf_path, reload.clone()).await {
                        Ok(()) => continue,
                        Err(e) => ("Could not open the editor".to_string(), e),
                    },
                };
                #[cfg(windows)]
                toasts.lock().unwrap().push(toast);
                #[cfg(not(windows))]
                let _ = toast;
            }
        }
    }
    drop(editor);
    run.abort();
    #[cfg(windows)]
    drop(tray);
    let _ = std::fs::remove_file(&pid_path);
    ExitCode::SUCCESS
}

/// Pick the prefix and configuration file. Running the binary directly
/// (no arguments) uses `conf/nginx.conf` next to the executable, like an
/// nginx installation; when that file doesn't exist (e.g. `cargo run`) the
/// working directory is used instead.
fn locate(prefix_arg: Option<PathBuf>, conf_arg: Option<PathBuf>) -> (PathBuf, PathBuf) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let explicit = prefix_arg.is_some();
    let prefix = prefix_arg
        .map(|p| if p.is_relative() { cwd.join(p) } else { p })
        .or_else(|| std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)))
        .unwrap_or_else(|| cwd.clone());
    match conf_arg {
        Some(c) => (prefix, c),
        None => {
            let c = prefix.join("conf").join("nginx.conf");
            if explicit || c.exists() {
                (prefix, c)
            } else {
                (cwd.clone(), cwd.join("conf").join("nginx.conf"))
            }
        }
    }
}

/// Read the configuration again and apply it to the running server.
async fn reload_config(reloader: &Reloader, conf_path: &Path, prefix: &Path, log: &daemon::LogHandle) -> Result<String, String> {
    let mut config = Config::load(conf_path).map_err(|e| {
        tracing::error!("reload failed, keeping the running configuration: {e}");
        format!("reload failed: {e}")
    })?;
    config.prefix = Some(prefix.to_path_buf());
    for w in &config.warnings {
        tracing::warn!("{w}");
    }
    let (path, level) = match &config.error_log {
        Some(el) => (resolve_path(Some(prefix), &el.path), el.level.clone()),
        None => (prefix.join(DEFAULT_ERROR_LOG), "error".to_string()),
    };
    reloader.reload(config).await.map_err(|e| format!("reload failed: {e}"))?;
    if let Err(e) = log.open(&path, &level) {
        tracing::warn!("{e}");
    }
    Ok("The running server uses the new configuration.".into())
}

/// Start the editor once and show it in its window (in the browser where
/// there is no window).
async fn open_editor(editor: &mut Option<editor::Running>, conf_path: &Path, apply: editor::Apply) -> Result<(), String> {
    if editor.is_none() {
        let e = editor::start(conf_path.to_path_buf(), 0, apply).await.map_err(|e| format!("editor: {e}"))?;
        *editor = Some(e);
    }
    let url = editor.as_ref().expect("started above").url.clone();
    show_editor(&url).await.map(drop)
}

/// The editor's window thread, `None` when an open window was brought to
/// the front or the page went to the browser.
type Shown = Option<std::thread::JoinHandle<()>>;

async fn show_editor(url: &str) -> Result<Shown, String> {
    #[cfg(windows)]
    {
        let u = url.to_string();
        match tokio::task::spawn_blocking(move || editor_window::open(&u)).await {
            Ok(Ok(shown)) => return Ok(shown),
            Ok(Err(e)) => tracing::warn!("editor window: {e}; using the browser"),
            Err(e) => tracing::warn!("editor window: {e}; using the browser"),
        }
    }
    if daemon::open_browser(url) { Ok(None) } else { Err(format!("open {url} in a browser")) }
}

/// `-e`: edit the configuration without starting a server; saving sends
/// `reload` to the running one, if any.
async fn edit_standalone(prefix: PathBuf, conf_path: PathBuf) -> ExitCode {
    let pid_path = {
        let pid = Config::load(&conf_path).ok().and_then(|c| c.pid);
        resolve_path(Some(&prefix), pid.as_deref().unwrap_or(DEFAULT_PID))
    };
    let apply: editor::Apply = Arc::new(move || {
        let pid_path = pid_path.clone();
        Box::pin(async move {
            match daemon::read_pid(&pid_path) {
                Err(_) => Ok("No running server to reload; it will use the new configuration when started.".into()),
                Ok(pid) => daemon::send_signal("reload", pid)
                    .map(|()| format!("Reload signal sent to the server (PID {pid}); problems go to its error log.")),
            }
        })
    });
    let running = match editor::start(conf_path.clone(), 0, apply).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nginx: editor: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("nginx: editing {}", conf_path.display());
    match show_editor(&running.url).await {
        // Done when the window is closed.
        Ok(Some(window)) => {
            tokio::select! {
                _ = tokio::task::spawn_blocking(move || window.join()) => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        _ => {
            println!("nginx: open {} (Ctrl+C to quit)", running.url);
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    ExitCode::SUCCESS
}
