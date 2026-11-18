use tonic::{Request, Response, Status};
use vmlayer_core::SnapshotId;
use vmlayer_proto::v1 as pb;
use vmlayer_proto::v1::system_server::System;

use super::convert::{self, status};
use super::Services;

#[tonic::async_trait]
impl System for Services {
    async fn info(&self, _req: Request<pb::Empty>) -> Result<Response<pb::SystemInfo>, Status> {
        let backends = self
            .daemon
            .registry
            .entries()
            .map(|(name, e)| pb::BackendInfo {
                name: name.clone(),
                engine_version: e.engine_version.clone(),
                capabilities: Some(convert::capabilities(e.backend.capabilities())),
                unavailable_reason: e.unavailable.clone().unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(pb::SystemInfo {
            daemon_version: crate::VERSION.to_string(),
            backends,
            default_backend: self.daemon.registry.default_name(),
            data_dir: self.daemon.cfg.data_dir.display().to_string(),
            blob_store: self
                .daemon
                .volumes
                .remote()
                .map(|r| r.describe().to_string())
                .unwrap_or_default(),
        }))
    }

    async fn list_hosts(
        &self,
        _req: Request<pb::Empty>,
    ) -> Result<Response<pb::ListHostsResponse>, Status> {
        let hosts = self.daemon.list_hosts().await.map_err(status)?;
        Ok(Response::new(pb::ListHostsResponse {
            hosts: hosts.into_iter().map(host_info).collect(),
        }))
    }

    async fn place_sandbox(
        &self,
        req: Request<pb::PlaceSandboxRequest>,
    ) -> Result<Response<pb::HostInfo>, Status> {
        let r = req.into_inner();
        let vcpus = if r.vcpus == 0 {
            self.daemon.cfg.defaults.vcpus
        } else {
            r.vcpus
        };
        let memory = if r.memory_mib == 0 {
            self.daemon.cfg.defaults.memory_mib
        } else {
            r.memory_mib
        };
        let host = self
            .daemon
            .place(
                &r.backend,
                vcpus,
                u64::from(memory),
                &convert::labels_from(&r.placement),
                &r.exclude_hosts,
            )
            .await
            .map_err(status)?;
        Ok(Response::new(host_info(host)))
    }

    async fn collect_garbage(
        &self,
        req: Request<pb::CollectGarbageRequest>,
    ) -> Result<Response<pb::CollectGarbageResponse>, Status> {
        let s = self
            .daemon
            .volumes
            .collect_garbage(req.into_inner().dry_run)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::CollectGarbageResponse {
            snapshots: s.manifests as u64,
            chunks: s.chunks as u64,
            referenced_chunks: s.referenced as u64,
            recent_chunks: s.recent as u64,
            deleted_chunks: s.deleted as u64,
            deleted_bytes: s.deleted_bytes,
        }))
    }

    async fn health(
        &self,
        _req: Request<pb::Empty>,
    ) -> Result<Response<pb::HealthResponse>, Status> {
        Ok(Response::new(pb::HealthResponse { ok: true }))
    }

    async fn list_snapshots(
        &self,
        req: Request<pb::ListSnapshotsRequest>,
    ) -> Result<Response<pb::ListSnapshotsResponse>, Status> {
        let r = req.into_inner();
        let labels = convert::labels_from(&r.labels);
        let snaps = if r.remote {
            self.daemon
                .list_remote_snapshots()
                .await
                .map_err(status)?
                .into_iter()
                .filter(|s| labels.iter().all(|(k, v)| s.labels.get(k) == Some(v)))
                .collect()
        } else {
            self.daemon.list_snapshots(&labels).map_err(status)?
        };
        Ok(Response::new(pb::ListSnapshotsResponse {
            snapshots: snaps.iter().map(convert::snapshot).collect(),
        }))
    }

    async fn pull_image(
        &self,
        req: Request<pb::PullImageRequest>,
    ) -> Result<Response<pb::ImageInfo>, Status> {
        let r = req.into_inner();
        convert::require_id(&r.image, "image")?;
        let (backend, entry) = self.daemon.registry.pick(&r.backend).map_err(status)?;
        let resolved = entry.resolver.resolve(&r.image).await.map_err(status)?;
        let (rootfs_kind, rootfs_path) = match &resolved.rootfs {
            vmlayer_core::Rootfs::EngineImage { .. } => ("engine", String::new()),
            vmlayer_core::Rootfs::Ext4 { path } => ("ext4", path.display().to_string()),
            vmlayer_core::Rootfs::Dir { path } => ("dir", path.display().to_string()),
        };
        Ok(Response::new(pb::ImageInfo {
            image: r.image,
            backend,
            rootfs_kind: rootfs_kind.to_string(),
            rootfs_path,
            entrypoint: resolved.entrypoint.unwrap_or_default(),
            workdir: resolved.workdir.unwrap_or_default(),
        }))
    }

    async fn delete_snapshot(
        &self,
        req: Request<pb::DeleteSnapshotRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        self.daemon
            .delete_snapshot(&SnapshotId(r.id), r.local_only)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }
}

fn host_info(h: crate::cluster::HostDoc) -> pb::HostInfo {
    pb::HostInfo {
        host_id: h.host_id,
        endpoint: h.endpoint,
        daemon_version: h.daemon_version,
        backends: h.backends,
        cpus: h.cpus,
        memory_mib: h.memory_mib,
        allocated_vcpus: h.allocated_vcpus,
        allocated_memory_mib: h.allocated_memory_mib,
        sandbox_ids: h.sandboxes,
        labels: h.labels.into_iter().collect(),
        updated_at: convert::ts(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(h.updated_at),
        ),
        allocatable_vcpus: h.allocatable_vcpus,
        allocatable_memory_mib: h.allocatable_memory_mib,
    }
}
