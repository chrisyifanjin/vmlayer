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
        let labels = convert::labels_from(&req.into_inner().labels);
        let snaps = self.daemon.list_snapshots(&labels).map_err(status)?;
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
            .delete_snapshot(&SnapshotId(r.id))
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }
}
