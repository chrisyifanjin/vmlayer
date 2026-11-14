use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use vmlayerd::config::Config;
use vmlayerd::manager::Daemon;
use vmlayerd::server::{parse_listen, serve};

/// vmlayerd: the vmlayer host daemon.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// TOML config file. Flags override it.
    #[arg(long, env = "VMLAYER_CONFIG")]
    config: Option<PathBuf>,
    /// unix:///path/to.sock or tcp://host:port
    #[arg(long, env = "VMLAYER_LISTEN")]
    listen: Option<String>,
    /// Bearer token, required for tcp listeners.
    #[arg(long, env = "VMLAYER_TOKEN")]
    token: Option<String>,
    #[arg(long, env = "VMLAYER_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// pretty or json
    #[arg(long, env = "VMLAYER_LOG_FORMAT")]
    log_format: Option<String>,
    /// tracing filter, e.g. info,vmlayerd=debug
    #[arg(long, env = "VMLAYER_LOG")]
    log_filter: Option<String>,
    /// Backend used for "auto" (docker, firecracker, ...).
    #[arg(long, env = "VMLAYER_DEFAULT_BACKEND")]
    default_backend: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut cfg = Config::load(args.config.as_deref())?;
    if let Some(l) = args.listen {
        cfg.listen = l;
    }
    if args.token.is_some() {
        cfg.token = args.token;
    }
    if let Some(d) = args.data_dir {
        cfg.data_dir = d;
    }
    if let Some(f) = args.log_format {
        cfg.log.format = f;
    }
    if let Some(f) = args.log_filter {
        cfg.log.filter = f;
    }
    if args.default_backend.is_some() {
        cfg.default_backend = args.default_backend;
    }

    let filter = tracing_subscriber::EnvFilter::try_new(&cfg.log.filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if cfg.log.format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    tracing::info!(version = vmlayerd::VERSION, data_dir = %cfg.data_dir.display(), "starting vmlayerd");

    let listen = parse_listen(&cfg.listen)?;
    let token = cfg.token.clone();
    let daemon: Arc<Daemon> = Daemon::new(cfg).await?;

    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("sigterm handler");
            tokio::select! {
                _ = ctrl_c => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        tracing::info!("shutting down");
    };
    serve(daemon, listen, token, shutdown).await
}
