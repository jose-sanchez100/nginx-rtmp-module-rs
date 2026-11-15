use rtmp_server::config::Config;
use rtmp_server::server::Server;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: rtmp-server [-c nginx.conf] [-t]

  -c FILE   configuration file (nginx syntax, rtmp {} block); default: conf/nginx.conf
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

    let mut conf_path = PathBuf::from("conf/nginx.conf");
    let mut test_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" => match args.next() {
                Some(p) => conf_path = p.into(),
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

    let config = match Config::load(&conf_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rtmp-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    if test_only {
        println!("rtmp-server: configuration file {} test is successful", conf_path.display());
        return ExitCode::SUCCESS;
    }

    let server = match Server::bind(config).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rtmp-server: bind failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::select! {
        _ = server.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    ExitCode::SUCCESS
}
