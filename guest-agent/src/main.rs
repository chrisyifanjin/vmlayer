//! vmlayer guest agent. Runs inside a microVM, normally as PID 1, and serves
//! the `vmlayer.v1.Processes`, `vmlayer.v1.Files` and `vmlayer.guest.v1.Guest`
//! gRPC services over vsock. A backend handle on the host is a client of it.
//!
//! `--listen tcp://0.0.0.0:PORT` exists so the agent can be exercised inside a
//! container (CI, laptops) where there is no vsock.

#[cfg(target_os = "linux")]
mod fs;
#[cfg(target_os = "linux")]
mod init;
mod netstat;
#[cfg(target_os = "linux")]
mod process;
#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod services;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Default vsock port the host connects to.
pub const DEFAULT_VSOCK_PORT: u32 = 5200;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use clap::Parser;

    #[derive(Parser)]
    #[command(version, about)]
    struct Args {
        /// vsock:PORT (default) or tcp://ADDR:PORT (for testing in a container).
        #[arg(long, env = "VMLAYER_AGENT_LISTEN")]
        listen: Option<String>,
        /// Do the PID 1 duties (mount /proc etc., reap orphans) even when not PID 1.
        #[arg(long)]
        init: bool,
        /// Skip the PID 1 duties even when PID 1.
        #[arg(long)]
        no_init: bool,
    }

    let args = Args::parse();
    let is_init = (std::process::id() == 1 || args.init) && !args.no_init;
    if is_init {
        init::early_boot();
        // Fork: the parent stays PID 1 reaping orphans and forwarding
        // signals; the child runs the agent. Tokio's own child handling
        // and a global waitpid(-1) reaper cannot share a process.
        if let Some(code) = init::fork_reaper() {
            std::process::exit(code);
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VMLAYER_AGENT_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let listen = args
        .listen
        .or_else(init::listen_from_cmdline)
        .unwrap_or_else(|| format!("vsock:{DEFAULT_VSOCK_PORT}"));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(server::serve(&listen))
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("vmlayer-guest-agent runs inside a Linux guest only");
    std::process::exit(1);
}
