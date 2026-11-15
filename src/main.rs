use rtmp_server::config::Config;
use rtmp_server::server::Server;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: rtmp-server [-p prefix] [-c nginx.conf] [-t]

  -p DIR    prefix: relative paths in the configuration (access_log) are
            resolved against it; default: the directory of the executable
  -c FILE   configuration file (nginx syntax, rtmp {} block);
            default: PREFIX/conf/nginx.conf, or conf/nginx.conf in the
            working directory when -p is not given and that file is missing
  -t        test configuration and exit

Logging is controlled by RUST_LOG (e.g. RUST_LOG=rtmp_server=debug).";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rtmp_server=info".into()),
        )
        .init();

    let mut conf_arg: Option<PathBuf> = None;
    let mut prefix_arg: Option<PathBuf> = None;
    let mut test_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "-p" => match args.next() {
                Some(p) if a == "-c" => conf_arg = Some(p.into()),
                Some(p) => prefix_arg = Some(p.into()),
                None => {
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
                eprintln!("unknown argument {other}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let (prefix, conf_path) = locate(prefix_arg, conf_arg);
    let mut config = match Config::load(&conf_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rtmp-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("configuration {}, prefix {}", conf_path.display(), prefix.display());
    config.prefix = Some(prefix);
    if test_only {
        if let Err(e) = rtmp_server::access_log::open_all(&mut config) {
            eprintln!("rtmp-server: {e}");
            return ExitCode::FAILURE;
        }
        println!("rtmp-server: configuration file {} test is successful", conf_path.display());
        return ExitCode::SUCCESS;
    }

    let server = match Server::bind(config).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rtmp-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::select! {
        _ = server.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
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
