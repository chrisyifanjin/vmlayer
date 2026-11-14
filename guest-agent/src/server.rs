//! Listener setup: vsock (production) or tcp (testing in a container).

use tonic::transport::Server;
use vmlayer_proto::guest::v1::guest_server::GuestServer;
use vmlayer_proto::v1::files_server::FilesServer;
use vmlayer_proto::v1::processes_server::ProcessesServer;

use crate::services::Agent;

const OUTPUT_BUFFER: usize = 8 * 1024 * 1024;

pub async fn serve(listen: &str) -> anyhow::Result<()> {
    let agent = Agent::new(OUTPUT_BUFFER);
    let routes = tonic::service::Routes::new(ProcessesServer::new(agent.clone()))
        .add_service(FilesServer::new(agent.clone()))
        .add_service(GuestServer::new(agent));
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
        tracing::info!("agent shutting down");
    };
    if let Some(port) = listen.strip_prefix("vsock:") {
        let port: u32 = port.parse()?;
        let addr = tokio_vsock::VsockAddr::new(tokio_vsock::VMADDR_CID_ANY, port);
        let listener = tokio_vsock::VsockListener::bind(addr)?;
        tracing::info!(
            port,
            version = crate::VERSION,
            "guest agent listening on vsock"
        );
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming_shutdown(listener.incoming(), shutdown)
            .await?;
    } else if let Some(addr) = listen.strip_prefix("tcp://") {
        let addr: std::net::SocketAddr = addr.parse()?;
        tracing::info!(%addr, version = crate::VERSION, "guest agent listening on tcp (test mode)");
        Server::builder()
            .add_routes(routes)
            .serve_with_shutdown(addr, shutdown)
            .await?;
    } else {
        anyhow::bail!("listen must be vsock:PORT or tcp://ADDR:PORT, got {listen:?}");
    }
    Ok(())
}
