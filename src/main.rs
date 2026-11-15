mod daemon;
#[cfg(windows)]
mod tray;

use nginx_rtmp::access_log::resolve_path;
use nginx_rtmp::config::Config;
use nginx_rtmp::server::Server;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: nginx [-p prefix] [-c nginx.conf] [-t] [-s stop|quit]

  -p DIR    prefix: relative paths in the configuration (logs, pid) are
            resolved against it; default: the directory of the executable
  -c FILE   configuration file (nginx syntax, rtmp {} block);
            default: PREFIX/conf/nginx.conf, or conf/nginx.conf in the
            working directory when -p is not given and that file is missing
  -t        test configuration and exit
  -s SIGNAL send a signal to the running server: stop, quit

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
    let serve = !test_only && signal.is_none();

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
    let mut config = match Config::load(&conf_path) {
        Ok(c) => c,
        Err(e) => return fail(e.to_string()),
    };
    config.prefix = Some(prefix.clone());
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
    let signals = match daemon::listen_signals() {
        Ok(s) => s,
        Err(e) => return fail(format!("signals: {e}")),
    };
    let stop = std::sync::Arc::new(tokio::sync::Notify::new());
    #[cfg(windows)]
    let tray = tray::start(tray::TrayOptions {
        registry: server.registry.clone(),
        listen: server.listen_info(),
        conf_path: conf_path.clone(),
        logs_dir: prefix.join("logs"),
        stop: stop.clone(),
    });
    #[cfg(windows)]
    if tray.is_none() {
        tracing::warn!("could not create the tray icon");
    }
    tokio::select! {
        _ = server.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
        sig = signals => tracing::info!("signal {sig} received, exiting"),
        _ = stop.notified() => tracing::info!("stopped from the tray icon"),
    }
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
