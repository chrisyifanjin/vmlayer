use tonic::{Request, Response, Status};
use vmlayer_core::VolumeSnapshotId;
use vmlayer_proto::v1 as pb;
use vmlayer_proto::v1::volumes_server::Volumes;

use super::convert::{self, status};
use super::Services;
use crate::auth::{scoped_volume_name, Principal};

#[tonic::async_trait]
impl Volumes for Services {
    async fn create(
        &self,
        req: Request<pb::CreateVolumeRequest>,
    ) -> Result<Response<pb::Volume>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        let mut labels = convert::labels_from(&r.labels);
        p.stamp(&mut labels);
        // A volume takes the representation of the backend that will mount
        // it: the requested one, else the daemon's default backend.
        let (_, entry) = self.daemon.registry.pick(&r.backend).map_err(status)?;
        let v = self
            .daemon
            .volumes
            .create_kind(
                entry.backend.volume_source_kind(),
                (!r.name.is_empty()).then(|| scoped_volume_name(r.name, &labels)),
                r.size_mib,
                labels,
                false,
            )
            .await
            .map_err(status)?;
        Ok(Response::new(convert::volume(&p, &v)))
    }

    async fn from_snapshot(
        &self,
        req: Request<pb::VolumeFromSnapshotRequest>,
    ) -> Result<Response<pb::Volume>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        convert::require_id(&r.snapshot_id, "snapshot_id")?;
        let source = self.volume_snapshot_labels(&p, &r.snapshot_id).await?;
        let mut labels = convert::labels_from(&r.labels);
        p.stamp(&mut labels);
        crate::auth::inherit_tenant(&mut labels, &source);
        let v = self
            .daemon
            .volumes
            .from_snapshot(
                &VolumeSnapshotId(r.snapshot_id),
                (!r.name.is_empty()).then(|| scoped_volume_name(r.name, &labels)),
                labels,
                false,
            )
            .await
            .map_err(status)?;
        Ok(Response::new(convert::volume(&p, &v)))
    }

    async fn get(
        &self,
        req: Request<pb::GetVolumeRequest>,
    ) -> Result<Response<pb::Volume>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        convert::require_id(&r.volume, "volume")?;
        let v = self.volume(&p, &r.volume)?;
        Ok(Response::new(convert::volume(&p, &v)))
    }

    async fn list(
        &self,
        req: Request<pb::ListVolumesRequest>,
    ) -> Result<Response<pb::ListVolumesResponse>, Status> {
        let p = Principal::of(&req);
        let mut labels = convert::labels_from(&req.into_inner().labels);
        p.stamp(&mut labels);
        let vols = self.daemon.volumes.list().map_err(status)?;
        Ok(Response::new(pb::ListVolumesResponse {
            volumes: vols
                .iter()
                .filter(|v| labels.iter().all(|(k, val)| v.labels.get(k) == Some(val)))
                .map(|v| convert::volume(&p, v))
                .collect(),
        }))
    }

    async fn delete(
        &self,
        req: Request<pb::DeleteVolumeRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        let v = self.volume(&p, &r.id)?;
        self.daemon.volumes.delete(&v.id).await.map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn take_snapshot(
        &self,
        req: Request<pb::SnapshotVolumeRequest>,
    ) -> Result<Response<pb::VolumeSnapshot>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        let v = self.volume(&p, &r.id)?;
        let mut labels = convert::labels_from(&r.labels);
        p.stamp(&mut labels);
        // Quiesce the holder's filesystem if the volume is attached read-write.
        let holder = match &v.attached_to {
            Some(sb) => self.daemon.live(sb).await.ok(),
            None => None,
        };
        if let Some(live) = &holder {
            let _ = live.handle.freeze_fs().await;
        }
        let guest_path = holder
            .as_ref()
            .and_then(|l| self.daemon.store.mounts_for(&l.id).ok())
            .and_then(|ms| {
                ms.into_iter()
                    .find(|m| m.volume_id == v.id)
                    .map(|m| m.guest_path)
            });
        let result = self
            .daemon
            .volumes
            .snapshot(&v.id, guest_path, labels, r.durable)
            .await;
        if let Some(live) = &holder {
            let _ = live.handle.thaw_fs().await;
        }
        let snap = result.map_err(status)?;
        Ok(Response::new(convert::volume_snapshot_row(&snap)))
    }

    async fn clone(
        &self,
        req: Request<pb::CloneVolumeRequest>,
    ) -> Result<Response<pb::Volume>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        let v = self.volume(&p, &r.id)?;
        let name = (!r.name.is_empty()).then(|| scoped_volume_name(r.name, &v.labels));
        let c = self
            .daemon
            .volumes
            .clone_volume(&v.id, name, false)
            .await
            .map_err(status)?;
        Ok(Response::new(convert::volume(&p, &c)))
    }

    async fn list_snapshots(
        &self,
        req: Request<pb::ListVolumeSnapshotsRequest>,
    ) -> Result<Response<pb::ListVolumeSnapshotsResponse>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        let mut labels = convert::labels_from(&r.labels);
        p.stamp(&mut labels);
        if r.remote {
            let snaps = self.daemon.volumes.list_remote().await.map_err(status)?;
            return Ok(Response::new(pb::ListVolumeSnapshotsResponse {
                snapshots: snaps
                    .iter()
                    .filter(|s| labels.iter().all(|(k, val)| s.labels.get(k) == Some(val)))
                    .map(convert::volume_snapshot_manifest)
                    .collect(),
            }));
        }
        let snaps = self.daemon.volumes.list_snapshots().map_err(status)?;
        Ok(Response::new(pb::ListVolumeSnapshotsResponse {
            snapshots: snaps
                .iter()
                .filter(|s| labels.iter().all(|(k, val)| s.labels.get(k) == Some(val)))
                .map(convert::volume_snapshot_row)
                .collect(),
        }))
    }

    async fn delete_snapshot(
        &self,
        req: Request<pb::DeleteVolumeSnapshotRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        self.volume_snapshot_labels(&p, &r.id).await?;
        self.daemon
            .volumes
            .delete_snapshot(&VolumeSnapshotId(r.id), r.local_only)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }
}
