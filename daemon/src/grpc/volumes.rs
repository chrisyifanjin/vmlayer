use tonic::{Request, Response, Status};
use vmlayer_core::{VolumeId, VolumeSnapshotId};
use vmlayer_proto::v1 as pb;
use vmlayer_proto::v1::volumes_server::Volumes;

use super::convert::{self, status};
use super::Services;

#[tonic::async_trait]
impl Volumes for Services {
    async fn create(
        &self,
        req: Request<pb::CreateVolumeRequest>,
    ) -> Result<Response<pb::Volume>, Status> {
        let r = req.into_inner();
        // A volume takes the representation of the backend that will mount
        // it: the requested one, else the daemon's default backend.
        let (_, entry) = self.daemon.registry.pick(&r.backend).map_err(status)?;
        let v = self
            .daemon
            .volumes
            .create_kind(
                entry.backend.volume_source_kind(),
                (!r.name.is_empty()).then_some(r.name),
                r.size_mib,
                convert::labels_from(&r.labels),
                false,
            )
            .await
            .map_err(status)?;
        Ok(Response::new(convert::volume(&v)))
    }

    async fn from_snapshot(
        &self,
        req: Request<pb::VolumeFromSnapshotRequest>,
    ) -> Result<Response<pb::Volume>, Status> {
        let r = req.into_inner();
        convert::require_id(&r.snapshot_id, "snapshot_id")?;
        let v = self
            .daemon
            .volumes
            .from_snapshot(
                &VolumeSnapshotId(r.snapshot_id),
                (!r.name.is_empty()).then_some(r.name),
                convert::labels_from(&r.labels),
                false,
            )
            .await
            .map_err(status)?;
        Ok(Response::new(convert::volume(&v)))
    }

    async fn get(
        &self,
        req: Request<pb::GetVolumeRequest>,
    ) -> Result<Response<pb::Volume>, Status> {
        let r = req.into_inner();
        convert::require_id(&r.volume, "volume")?;
        let v = self.daemon.volumes.get(&r.volume).map_err(status)?;
        Ok(Response::new(convert::volume(&v)))
    }

    async fn list(
        &self,
        req: Request<pb::ListVolumesRequest>,
    ) -> Result<Response<pb::ListVolumesResponse>, Status> {
        let labels = convert::labels_from(&req.into_inner().labels);
        let vols = self.daemon.volumes.list().map_err(status)?;
        Ok(Response::new(pb::ListVolumesResponse {
            volumes: vols
                .iter()
                .filter(|v| labels.iter().all(|(k, val)| v.labels.get(k) == Some(val)))
                .map(convert::volume)
                .collect(),
        }))
    }

    async fn delete(
        &self,
        req: Request<pb::DeleteVolumeRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        let v = self.daemon.volumes.get(&r.id).map_err(status)?;
        self.daemon.volumes.delete(&v.id).await.map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn take_snapshot(
        &self,
        req: Request<pb::SnapshotVolumeRequest>,
    ) -> Result<Response<pb::VolumeSnapshot>, Status> {
        let r = req.into_inner();
        let v = self.daemon.volumes.get(&r.id).map_err(status)?;
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
            .snapshot(
                &v.id,
                guest_path,
                convert::labels_from(&r.labels),
                r.durable,
            )
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
        let r = req.into_inner();
        let v = self.daemon.volumes.get(&r.id).map_err(status)?;
        let c = self
            .daemon
            .volumes
            .clone_volume(&v.id, (!r.name.is_empty()).then_some(r.name), false)
            .await
            .map_err(status)?;
        Ok(Response::new(convert::volume(&c)))
    }

    async fn list_snapshots(
        &self,
        req: Request<pb::ListVolumeSnapshotsRequest>,
    ) -> Result<Response<pb::ListVolumeSnapshotsResponse>, Status> {
        let r = req.into_inner();
        let labels = convert::labels_from(&r.labels);
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
        let r = req.into_inner();
        self.daemon
            .volumes
            .delete_snapshot(&VolumeSnapshotId(r.id), r.local_only)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }
}

#[allow(dead_code)]
fn _unused(_: VolumeId) {}
