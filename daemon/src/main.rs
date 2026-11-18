use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use vmlayerd::config::Config;
use vmlayerd::manager::Daemon;
use vmlayerd::server::{parse_listen, serve, Security};

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
    /// Administrator bearer token for tcp listeners. `[auth] tokens` in the
    /// config file adds tokens with a tenant and a role.
    #[arg(long, env = "VMLAYER_TOKEN")]
    token: Option<String>,
    /// PEM certificate for TLS on tcp listeners.
    #[arg(long, env = "VMLAYER_TLS_CERT")]
    tls_cert: Option<PathBuf>,
    /// PEM private key for TLS on tcp listeners.
    #[arg(long, env = "VMLAYER_TLS_KEY")]
    tls_key: Option<PathBuf>,
    /// PEM CA bundle; callers must present a certificate it signed.
    #[arg(long, env = "VMLAYER_TLS_CLIENT_CA")]
    tls_client_ca: Option<PathBuf>,
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
    /// Blob store for durable volume snapshots, e.g. s3://bucket/prefix.
    #[arg(long, env = "VMLAYER_BLOB_STORE")]
    blob_store: Option<String>,
    /// This host's name in the host registry (default: hostname).
    #[arg(long, env = "VMLAYER_HOST_ID")]
    host_id: Option<String>,
    /// Address to publish in the host registry, e.g. tcp://10.0.0.5:7070.
    #[arg(long, env = "VMLAYER_ADVERTISE")]
    advertise: Option<String>,
    #[arg(long, env = "VMLAYER_HEARTBEAT_SECS")]
    heartbeat_secs: Option<u64>,
    #[arg(long, env = "VMLAYER_LEASE_TTL_SECS")]
    lease_ttl_secs: Option<u64>,
    /// Bring back rescheduled sandboxes of failed hosts on this one.
    #[arg(long, env = "VMLAYER_RESCUE")]
    rescue: bool,
    #[arg(long, env = "VMLAYER_CHECKPOINT_SECS")]
    checkpoint_secs: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Two TLS crypto providers are linked (object_store brings aws-lc-rs);
    // rustls refuses to guess, so pick one for the whole process.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    let mut cfg = Config::load(args.config.as_deref())?;
    if let Some(l) = args.listen {
        cfg.listen = l;
    }
    if args.token.is_some() {
        cfg.token = args.token;
    }
    if args.tls_cert.is_some() {
        cfg.tls.cert = args.tls_cert;
    }
    if args.tls_key.is_some() {
        cfg.tls.key = args.tls_key;
    }
    if args.tls_client_ca.is_some() {
        cfg.tls.client_ca = args.tls_client_ca;
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
    if args.blob_store.is_some() {
        cfg.blob_store.url = args.blob_store;
    }
    if args.host_id.is_some() {
        cfg.cluster.host_id = args.host_id;
    }
    if args.advertise.is_some() {
        cfg.cluster.advertise = args.advertise;
    }
    if let Some(s) = args.heartbeat_secs {
        cfg.cluster.heartbeat_secs = s;
    }
    if let Some(s) = args.lease_ttl_secs {
        cfg.cluster.lease_ttl_secs = s;
    }
    if args.rescue {
        cfg.cluster.rescue = true;
    }
    if let Some(s) = args.checkpoint_secs {
        cfg.cluster.checkpoint_secs = s;
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
    cfg.credential_proxy.validate()?;
    let security = Security {
        token: cfg.token.clone(),
        auth: cfg.auth.clone(),
        tls: cfg.tls.clone(),
    };
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
    tokio::spawn({
        let daemon = daemon.clone();
        async move {
            if let Err(e) = vmlayerd::metrics::serve(daemon).await {
                tracing::error!(error = %format!("{e:#}"), "metrics endpoint stopped");
            }
        }
    });
    tokio::spawn({
        let daemon = daemon.clone();
        async move {
            if let Err(e) = vmlayerd::credentials::serve(daemon).await {
                tracing::error!(error = %format!("{e:#}"), "credential proxy stopped");
            }
        }
    });
    let serving = serve(daemon.clone(), listen, security, shutdown);
    let result = if daemon.cfg.gateway.enabled() {
        // The gateway serves until the process exits; it only returns on a
        // startup error, which stops the daemon too.
        let gateway = tokio::spawn(vmlayerd::gateway::serve(
            daemon.clone(),
            daemon.cfg.gateway.clone(),
        ));
        tokio::select! {
            r = serving => r,
            r = gateway => match r {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.context("port gateway")),
                Err(e) => Err(anyhow::anyhow!("port gateway: {e}")),
            },
        }
    } else {
        serving.await
    };
    daemon.withdraw_heartbeat().await;
    result
}
