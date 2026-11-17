use std::time::Duration;

use tonic::{Request, Response, Status};
use vmlayer_core::{SandboxId, SnapshotId};
use vmlayer_proto::v1 as pb;
use vmlayer_proto::v1::sandboxes_server::Sandboxes;

use super::convert::{self, status};
use super::Services;
use crate::manager::{CreateParams, EntrypointSpec, MountParam, WorkspaceSpec};

fn port(p: u32) -> Result<u16, Status> {
    u16::try_from(p)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| Status::invalid_argument("port must be 1..65535"))
}

#[tonic::async_trait]
impl Sandboxes for Services {
    async fn create(
        &self,
        req: Request<pb::CreateSandboxRequest>,
    ) -> Result<Response<pb::Sandbox>, Status> {
        let r = req.into_inner();
        convert::require_id(&r.image, "image")?;
        let workspace = match r.workspace.and_then(|w| w.kind) {
            None | Some(pb::workspace_spec::Kind::Anonymous(_)) => WorkspaceSpec::Anonymous,
            Some(pb::workspace_spec::Kind::Volume(v)) => WorkspaceSpec::Volume(v),
            Some(pb::workspace_spec::Kind::None(_)) => WorkspaceSpec::None,
        };
        let entrypoint = match r.entrypoint.and_then(|e| e.kind) {
            None | Some(pb::entrypoint_spec::Kind::ImageDefault(_)) => EntrypointSpec::ImageDefault,
            Some(pb::entrypoint_spec::Kind::Argv(a)) => EntrypointSpec::Argv(a.argv),
            Some(pb::entrypoint_spec::Kind::None(_)) => EntrypointSpec::None,
        };
        let params = CreateParams {
            image: r.image,
            vcpus: (r.vcpus > 0).then_some(r.vcpus),
            memory_mib: (r.memory_mib > 0).then_some(r.memory_mib),
            disk_mib: r.disk_mib,
            env: convert::labels_from(&r.env),
            workdir: (!r.workdir.is_empty()).then_some(r.workdir),
            workspace,
            mounts: r
                .mounts
                .into_iter()
                .map(|m| MountParam {
                    volume: m.volume_id,
                    guest_path: m.guest_path,
                    read_only: m.read_only,
                })
                .collect(),
            entrypoint,
            ttl: convert::dur_from(r.ttl),
            backend: r.backend,
            network: convert::network_from(r.network),
            labels: convert::labels_from(&r.labels),
            egress: convert::egress_from(r.egress)?,
            limits: convert::limits_from(r.limits)?,
        };
        let view = self.daemon.create(params).await.map_err(status)?;
        Ok(Response::new(convert::sandbox(&view)))
    }

    async fn restore(
        &self,
        req: Request<pb::RestoreSandboxRequest>,
    ) -> Result<Response<pb::Sandbox>, Status> {
        let r = req.into_inner();
        convert::require_id(&r.snapshot_id, "snapshot_id")?;
        let view = self
            .daemon
            .restore(
                &SnapshotId(r.snapshot_id),
                convert::dur_from(r.ttl),
                convert::labels_from(&r.labels),
            )
            .await
            .map_err(status)?;
        Ok(Response::new(convert::sandbox(&view)))
    }

    async fn get(
        &self,
        req: Request<pb::GetSandboxRequest>,
    ) -> Result<Response<pb::Sandbox>, Status> {
        let id = req.into_inner().id;
        convert::require_id(&id, "id")?;
        let view = self.daemon.view(&SandboxId(id)).await.map_err(status)?;
        Ok(Response::new(convert::sandbox(&view)))
    }

    async fn list(
        &self,
        req: Request<pb::ListSandboxesRequest>,
    ) -> Result<Response<pb::ListSandboxesResponse>, Status> {
        let labels = convert::labels_from(&req.into_inner().labels);
        let views = self.daemon.list(&labels).await.map_err(status)?;
        Ok(Response::new(pb::ListSandboxesResponse {
            sandboxes: views.iter().map(convert::sandbox).collect(),
        }))
    }

    async fn kill(
        &self,
        req: Request<pb::KillSandboxRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let id = req.into_inner().id;
        convert::require_id(&id, "id")?;
        match self.daemon.kill(&SandboxId(id)).await {
            Ok(()) | Err(vmlayer_core::Error::SandboxNotFound(_)) => {
                Ok(Response::new(pb::Empty {}))
            }
            Err(e) => Err(status(e)),
        }
    }

    async fn pause(
        &self,
        req: Request<pb::PauseSandboxRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        self.daemon
            .pause(&SandboxId(req.into_inner().id))
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn resume(
        &self,
        req: Request<pb::ResumeSandboxRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        self.daemon
            .resume(&SandboxId(req.into_inner().id))
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn take_snapshot(
        &self,
        req: Request<pb::SnapshotSandboxRequest>,
    ) -> Result<Response<pb::Snapshot>, Status> {
        let r = req.into_inner();
        convert::require_id(&r.id, "id")?;
        let snap = self
            .daemon
            .snapshot(
                &SandboxId(r.id),
                convert::labels_from(&r.labels),
                r.stay_paused,
            )
            .await
            .map_err(status)?;
        Ok(Response::new(convert::snapshot(&snap)))
    }

    async fn fork(
        &self,
        req: Request<pb::ForkSandboxRequest>,
    ) -> Result<Response<pb::ForkSandboxResponse>, Status> {
        let r = req.into_inner();
        convert::require_id(&r.id, "id")?;
        let views = self
            .daemon
            .fork(&SandboxId(r.id), r.count.max(1))
            .await
            .map_err(status)?;
        Ok(Response::new(pb::ForkSandboxResponse {
            children: views.iter().map(convert::sandbox).collect(),
        }))
    }

    async fn expose(
        &self,
        req: Request<pb::ExposeRequest>,
    ) -> Result<Response<pb::Endpoint>, Status> {
        let r = req.into_inner();
        let ep = self
            .daemon
            .expose(&SandboxId(r.id), port(r.guest_port)?)
            .await
            .map_err(status)?;
        Ok(Response::new(convert::endpoint(&ep)))
    }

    async fn unexpose(
        &self,
        req: Request<pb::UnexposeRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        self.daemon
            .unexpose(&SandboxId(r.id), port(r.guest_port)?)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn wait_for_port(
        &self,
        req: Request<pb::WaitForPortRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        let live = self.daemon.live(&SandboxId(r.id)).await.map_err(status)?;
        let timeout = convert::dur_from(r.timeout).unwrap_or(Duration::from_secs(30));
        live.handle
            .wait_for_port(port(r.port)?, timeout)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn attach(&self, req: Request<pb::AttachRequest>) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        let m = r
            .mount
            .ok_or_else(|| Status::invalid_argument("mount is required"))?;
        let live = self
            .daemon
            .live(&SandboxId(r.id.clone()))
            .await
            .map_err(status)?;
        let vol = self.daemon.volumes.get(&m.volume_id).map_err(status)?;
        self.daemon
            .volumes
            .attach(&vol.id, &live.id, m.read_only)
            .map_err(status)?;
        let source = self.daemon.volumes.source(&vol);
        live.handle
            .attach(vmlayer_core::Mount {
                volume: vol.id,
                source,
                guest_path: m.guest_path,
                read_only: m.read_only,
            })
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn detach(&self, req: Request<pb::DetachRequest>) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        let live = self.daemon.live(&SandboxId(r.id)).await.map_err(status)?;
        live.handle.detach(&r.guest_path).await.map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }
}
