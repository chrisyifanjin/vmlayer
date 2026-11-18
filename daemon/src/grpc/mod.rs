//! gRPC surface: one module per service plus conversions. Services are thin;
//! policy lives in `manager`.

pub mod convert;
pub mod files;
pub mod processes;
pub mod sandboxes;
pub mod system;
pub mod volumes;

use std::collections::BTreeMap;
use std::sync::Arc;

use tonic::Status;
use vmlayer_core::{Error, SandboxId, SnapshotId, VolumeId, VolumeSnapshotId};

use crate::auth::Principal;
use crate::manager::Daemon;
use crate::store::{StoreError, VolumeRow};

#[derive(Clone)]
pub struct Services {
    pub daemon: Arc<Daemon>,
}

pub fn router(daemon: Arc<Daemon>) -> tonic::service::Routes {
    use vmlayer_proto::v1::{
        files_server::FilesServer, processes_server::ProcessesServer,
        sandboxes_server::SandboxesServer, system_server::SystemServer,
        volumes_server::VolumesServer,
    };
    let s = Services { daemon };
    tonic::service::Routes::new(SandboxesServer::new(s.clone()))
        .add_service(ProcessesServer::new(s.clone()))
        .add_service(FilesServer::new(s.clone()))
        .add_service(VolumesServer::new(s.clone()))
        .add_service(SystemServer::new(s))
}

/// The error for a resource another tenant owns: exactly the one for a
/// resource that does not exist, so a caller cannot probe for IDs.
fn hidden(what: &str, id: &str) -> Status {
    convert::status(match what {
        "sandbox" => Error::SandboxNotFound(SandboxId(id.to_string())),
        "volume" => Error::VolumeNotFound(VolumeId(id.to_string())),
        "snapshot" => Error::SnapshotNotFound(SnapshotId(id.to_string())),
        _ => Error::Guest(format!("{what} {id} not found")),
    })
}

/// Tenant checks shared by the services (decision 0020). A tenant gets
/// `NotFound` for another tenant's resources, as if they did not exist.
impl Services {
    /// Sandbox `id`, if `p` may use it.
    pub(crate) fn sandbox(&self, p: &Principal, id: &str) -> Result<SandboxId, Status> {
        let sid = SandboxId(id.to_string());
        if p.tenant.is_none() {
            return Ok(sid);
        }
        match self.daemon.store.get_sandbox(&sid) {
            Ok(row) if p.sees(&row.labels) => Ok(sid),
            Ok(_) | Err(StoreError::NotFound) => Err(hidden("sandbox", id)),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    /// Volume by ID or name, if `p` may use it. A tenant's volume names
    /// are its own: `ws` means `tenant/ws`.
    pub(crate) fn volume(&self, p: &Principal, id_or_name: &str) -> Result<VolumeRow, Status> {
        let Some(t) = &p.tenant else {
            return self.daemon.volumes.get(id_or_name).map_err(convert::status);
        };
        [id_or_name.to_string(), format!("{t}/{id_or_name}")]
            .iter()
            .filter_map(|key| self.daemon.volumes.get(key).ok())
            .find(|v| p.sees(&v.labels))
            .ok_or_else(|| hidden("volume", id_or_name))
    }

    /// Labels of volume snapshot `id`, here or in the blob store, if `p`
    /// may use it. Empty when the local administrator names a snapshot
    /// that does not exist: the operation itself then reports it.
    pub(crate) async fn volume_snapshot_labels(
        &self,
        p: &Principal,
        id: &str,
    ) -> Result<BTreeMap<String, String>, Status> {
        let labels = self
            .daemon
            .volumes
            .snapshot_labels(&VolumeSnapshotId(id.to_string()))
            .await;
        self.visible(p, labels, "volume snapshot", id)
    }

    /// Labels of sandbox snapshot `id`, here or in the blob store, if `p`
    /// may use it.
    pub(crate) async fn snapshot_labels(
        &self,
        p: &Principal,
        id: &str,
    ) -> Result<BTreeMap<String, String>, Status> {
        let sid = SnapshotId(id.to_string());
        let labels = match self.daemon.store.get_snapshot(&sid) {
            Ok(s) => Some(s.labels),
            Err(_) if self.daemon.volumes.remote().is_some() => self
                .daemon
                .list_remote_snapshots()
                .await
                .ok()
                .and_then(|all| all.into_iter().find(|s| s.id == sid))
                .map(|s| s.labels),
            Err(_) => None,
        };
        self.visible(p, labels, "snapshot", id)
    }

    fn visible(
        &self,
        p: &Principal,
        labels: Option<BTreeMap<String, String>>,
        what: &str,
        id: &str,
    ) -> Result<BTreeMap<String, String>, Status> {
        match labels {
            Some(l) if p.sees(&l) => Ok(l),
            None if p.tenant.is_none() => Ok(BTreeMap::new()),
            _ => Err(hidden(what, id)),
        }
    }
}
