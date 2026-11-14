//! Port exposure through a `socat` helper container.
//!
//! Docker cannot add a port mapping to a running container, and a container
//! that shares another's network namespace cannot publish ports. So each
//! exposed port is a tiny helper container on the same bridge with a
//! published host port, forwarding to the sandbox's bridge address.

use std::collections::HashMap;

use bollard::query_parameters::{CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder};
use vmlayer_core::{Endpoint, Error, Result};

use crate::sandbox::DockerSandbox;
use crate::{engine_err, BACKEND_NAME, LABEL_MANAGED, LABEL_ROLE, LABEL_SANDBOX_ID};

async fn sandbox_ip(sb: &DockerSandbox) -> Result<String> {
    let info = sb
        .docker
        .inspect_container(&sb.container_id, None)
        .await
        .map_err(engine_err)?;
    info.network_settings
        .and_then(|n| n.networks)
        .and_then(|nets| {
            nets.into_values()
                .find_map(|e| e.ip_address.filter(|ip| !ip.is_empty()))
        })
        .ok_or_else(|| Error::Unsupported {
            backend: BACKEND_NAME,
            feature: "expose without a network",
        })
}

pub(crate) async fn expose(sb: &DockerSandbox, guest_port: u16) -> Result<Endpoint> {
    if let Some((_, ep)) = sb.exposed.lock().await.get(&guest_port) {
        return Ok(ep.clone());
    }
    let ip = sandbox_ip(sb).await?;
    let mut labels = HashMap::new();
    labels.insert(LABEL_MANAGED.to_string(), "true".to_string());
    labels.insert(LABEL_SANDBOX_ID.to_string(), sb.id.0.clone());
    labels.insert(LABEL_ROLE.to_string(), "expose".to_string());
    let key = format!("{guest_port}/tcp");
    let mut port_bindings = HashMap::new();
    port_bindings.insert(
        key.clone(),
        Some(vec![bollard::models::PortBinding {
            host_ip: Some("127.0.0.1".into()),
            host_port: Some(String::new()),
        }]),
    );
    let body = bollard::models::ContainerCreateBody {
        image: Some(sb.config.socat_image.clone()),
        cmd: Some(vec![
            format!("TCP-LISTEN:{guest_port},fork,reuseaddr"),
            format!("TCP:{ip}:{guest_port}"),
        ]),
        exposed_ports: Some(vec![key.clone()]),
        labels: Some(labels),
        host_config: Some(bollard::models::HostConfig {
            port_bindings: Some(port_bindings),
            network_mode: Some(sb.config.network.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    // Pull the helper image once.
    if sb
        .docker
        .inspect_image(&sb.config.socat_image)
        .await
        .is_err()
    {
        use futures::StreamExt;
        let opts = bollard::query_parameters::CreateImageOptionsBuilder::new()
            .from_image(&sb.config.socat_image)
            .build();
        let mut s = sb.docker.create_image(Some(opts), None, None);
        while let Some(i) = s.next().await {
            i.map_err(engine_err)?;
        }
    }
    let opts = CreateContainerOptionsBuilder::new()
        .name(&format!("vmlayer-{}-port-{guest_port}", sb.id))
        .build();
    let created = sb
        .docker
        .create_container(Some(opts), body)
        .await
        .map_err(engine_err)?;
    sb.docker
        .start_container(&created.id, None)
        .await
        .map_err(engine_err)?;
    let info = sb
        .docker
        .inspect_container(&created.id, None)
        .await
        .map_err(engine_err)?;
    let host_port = info
        .network_settings
        .and_then(|n| n.ports)
        .and_then(|p| p.get(&key).cloned().flatten())
        .and_then(|b| b.into_iter().find_map(|b| b.host_port))
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| Error::Guest("helper container got no host port".into()))?;
    let ep = Endpoint {
        host: "127.0.0.1".into(),
        port: host_port,
    };
    // The engine (and VM-based engines like colima or Docker Desktop in
    // particular) publishes the port a little after the container starts.
    // An endpoint we hand out must be connectable.
    wait_connectable(&ep, std::time::Duration::from_secs(10)).await?;
    sb.exposed
        .lock()
        .await
        .insert(guest_port, (created.id, ep.clone()));
    Ok(ep)
}

pub(crate) async fn unexpose(sb: &DockerSandbox, guest_port: u16) -> Result<()> {
    if let Some((cid, _)) = sb.exposed.lock().await.remove(&guest_port) {
        sb.docker
            .remove_container(
                &cid,
                Some(RemoveContainerOptionsBuilder::new().force(true).build()),
            )
            .await
            .map_err(engine_err)?;
    }
    Ok(())
}

async fn wait_connectable(ep: &Endpoint, timeout: std::time::Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::net::TcpStream::connect((ep.host.as_str(), ep.port)).await {
            Ok(_) => return Ok(()),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => {
                return Err(Error::Engine {
                    backend: BACKEND_NAME,
                    message: format!("exposed port {} never became reachable: {e}", ep.port),
                    source: None,
                })
            }
        }
    }
}
