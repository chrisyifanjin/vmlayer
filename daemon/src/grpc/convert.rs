//! Conversions between core types, store rows and proto messages, and the
//! one place core errors become gRPC statuses.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use tonic::Status;
use vmlayer_core::{
    Capabilities, Endpoint, Error, FileInfo, NetworkMode, ProcessInfo, SandboxState, SnapshotRef,
    VolumeSnapshotRef,
};
use vmlayer_proto::v1 as pb;

use crate::manager::SandboxView;
use crate::store::{VolumeRow, VolumeSnapshotRow};

pub fn status(e: Error) -> Status {
    let kind = match &e {
        Error::Unsupported { .. } => "unsupported",
        Error::SandboxNotFound(_) => "sandbox_not_found",
        Error::SnapshotNotFound(_) => "snapshot_not_found",
        Error::VolumeNotFound(_) => "volume_not_found",
        Error::VolumeBusy { .. } => "volume_busy",
        Error::InvalidState { .. } => "invalid_state",
        Error::SnapshotIncompatible { .. } => "snapshot_incompatible",
        Error::Timeout(_) => "timeout",
        Error::ResourceExhausted(_) => "resource_exhausted",
        Error::Guest(_) => "guest",
        Error::Engine { .. } => "engine",
        Error::Io(_) => "io",
    };
    let msg = e.to_string();
    let mut st = match e {
        Error::Unsupported { .. } => Status::unimplemented(msg),
        Error::SandboxNotFound(_) | Error::SnapshotNotFound(_) | Error::VolumeNotFound(_) => {
            Status::not_found(msg)
        }
        Error::VolumeBusy { .. }
        | Error::InvalidState { .. }
        | Error::SnapshotIncompatible { .. } => Status::failed_precondition(msg),
        Error::Timeout(_) => Status::deadline_exceeded(msg),
        Error::ResourceExhausted(_) => Status::resource_exhausted(msg),
        Error::Guest(_) => Status::failed_precondition(msg),
        Error::Engine { .. } | Error::Io(_) => Status::internal(msg),
    };
    st.metadata_mut()
        .insert("vmlayer-error", kind.parse().unwrap());
    st
}

pub fn ts(t: SystemTime) -> Option<prost_types::Timestamp> {
    Some(prost_types::Timestamp::from(t))
}

pub fn dur(d: Duration) -> Option<prost_types::Duration> {
    prost_types::Duration::try_from(d).ok()
}

pub fn dur_from(d: Option<prost_types::Duration>) -> Option<Duration> {
    d.and_then(|d| Duration::try_from(d).ok())
        .filter(|d| !d.is_zero())
}

pub fn labels(m: &BTreeMap<String, String>) -> std::collections::HashMap<String, String> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

pub fn labels_from(m: &std::collections::HashMap<String, String>) -> BTreeMap<String, String> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

pub fn capabilities(c: Capabilities) -> pb::Capabilities {
    pb::Capabilities {
        snapshot: c.snapshot,
        memory_snapshot: c.memory_snapshot,
        fork: c.fork,
        pause: c.pause,
        pty: c.pty,
        expose_port: c.expose_port,
        volumes: c.volumes,
        hot_attach: c.hot_attach,
        egress_policy: c.egress_policy,
        io_limits: c.io_limits,
    }
}

pub fn egress_from(
    e: Option<pb::EgressPolicy>,
) -> Result<Option<vmlayer_core::EgressPolicy>, Status> {
    e.map(|e| vmlayer_core::EgressPolicy::parse(&e.allow).map_err(Status::invalid_argument))
        .transpose()
}

pub fn limits_from(l: Option<pb::IoLimits>) -> Result<vmlayer_core::IoLimits, Status> {
    let l = l.unwrap_or_default();
    for (name, v) in [
        ("network_mbps", l.network_mbps),
        ("disk_mbps", l.disk_mbps),
        ("disk_iops", l.disk_iops),
    ] {
        if v == Some(0) {
            return Err(Status::invalid_argument(format!(
                "limits.{name} must be positive; leave it unset for no limit"
            )));
        }
    }
    Ok(vmlayer_core::IoLimits {
        network_mbps: l.network_mbps,
        disk_mbps: l.disk_mbps,
        disk_iops: l.disk_iops,
    })
}

pub fn state(s: SandboxState) -> i32 {
    match s {
        SandboxState::Creating => pb::SandboxState::Creating,
        SandboxState::Running => pb::SandboxState::Running,
        SandboxState::Paused => pb::SandboxState::Paused,
        SandboxState::Stopped => pb::SandboxState::Stopped,
        SandboxState::Failed => pb::SandboxState::Failed,
    }
    .into()
}

pub fn network_from(n: i32) -> Option<NetworkMode> {
    match pb::NetworkMode::try_from(n).ok()? {
        pb::NetworkMode::None => Some(NetworkMode::None),
        pb::NetworkMode::Egress => Some(NetworkMode::Egress),
        pb::NetworkMode::EgressAndExpose => Some(NetworkMode::EgressAndExpose),
        pb::NetworkMode::Unspecified => None,
    }
}

pub fn endpoint(e: &Endpoint) -> pb::Endpoint {
    pb::Endpoint {
        host: e.host.clone(),
        port: u32::from(e.port),
    }
}

pub fn sandbox(v: &SandboxView) -> pb::Sandbox {
    pb::Sandbox {
        id: v.row.id.0.clone(),
        backend: v.row.backend.clone(),
        state: state(v.state),
        capabilities: Some(capabilities(v.capabilities)),
        image: v.row.image.clone(),
        workdir: v.row.workdir.clone(),
        workspace_volume_id: v
            .row
            .workspace_volume_id
            .as_ref()
            .map(|i| i.0.clone())
            .unwrap_or_default(),
        mounts: v
            .mounts
            .iter()
            .map(|m| pb::Mount {
                volume_id: m.volume_id.0.clone(),
                guest_path: m.guest_path.clone(),
                read_only: m.read_only,
            })
            .collect(),
        created_at: ts(v.row.created_at),
        labels: labels(&v.row.labels),
        exposed: v
            .exposed
            .iter()
            .map(|(p, e)| (u32::from(*p), endpoint(e)))
            .collect(),
    }
}

pub fn volume(p: &crate::auth::Principal, v: &VolumeRow) -> pb::Volume {
    pb::Volume {
        id: v.id.0.clone(),
        name: v
            .name
            .as_deref()
            .map(|n| p.volume_name(n).to_string())
            .unwrap_or_default(),
        size_mib: v.size_mib,
        labels: labels(&v.labels),
        attached_to: v
            .attached_to
            .as_ref()
            .map(|s| s.0.clone())
            .unwrap_or_default(),
        anonymous: v.anonymous,
        created_at: ts(v.created_at),
    }
}

pub fn volume_snapshot_row(v: &VolumeSnapshotRow) -> pb::VolumeSnapshot {
    pb::VolumeSnapshot {
        id: v.id.0.clone(),
        volume_id: v.volume_id.0.clone(),
        guest_path: v.guest_path.clone().unwrap_or_default(),
        created_at: ts(v.created_at),
        size_bytes: v.size_bytes,
        durable: v.durable,
        labels: labels(&v.labels),
        local: v.path.is_dir(),
    }
}

/// A snapshot as the blob store describes it.
pub fn volume_snapshot_manifest(m: &vmlayer_blob::Manifest) -> pb::VolumeSnapshot {
    pb::VolumeSnapshot {
        id: m.id.clone(),
        volume_id: m.volume_id.clone(),
        guest_path: m.guest_path.clone().unwrap_or_default(),
        created_at: ts(SystemTime::UNIX_EPOCH + Duration::from_secs(m.created_at)),
        size_bytes: m.stored_bytes(),
        durable: true,
        labels: labels(&m.labels),
        local: false,
    }
}

pub fn volume_snapshot_ref(v: &VolumeSnapshotRef) -> pb::VolumeSnapshot {
    pb::VolumeSnapshot {
        id: v.id.0.clone(),
        volume_id: v.volume.0.clone(),
        guest_path: v.guest_path.clone().unwrap_or_default(),
        created_at: ts(v.created_at),
        size_bytes: v.size_bytes,
        durable: v.durable,
        labels: labels(&v.labels),
        local: true,
    }
}

pub fn snapshot(s: &SnapshotRef) -> pb::Snapshot {
    pb::Snapshot {
        id: s.id.0.clone(),
        backend: s.backend.clone(),
        engine_version: s.engine_version.clone(),
        cpu_model: s.cpu_model.clone().unwrap_or_default(),
        created_at: ts(s.created_at),
        size_bytes: s.size_bytes,
        includes_memory: s.includes_memory,
        volumes: s.volumes.iter().map(volume_snapshot_ref).collect(),
        labels: labels(&s.labels),
        durable: s.durable,
    }
}

pub use vmlayer_procs::grpc::{exit_status, output_chunk};

pub fn process_info(p: &ProcessInfo) -> pb::ProcessInfo {
    pb::ProcessInfo {
        pid: p.pid,
        argv: p.argv.clone(),
        started_at: ts(p.started_at),
        is_entrypoint: p.is_entrypoint,
    }
}

pub fn file_info(f: &FileInfo) -> pb::FileInfo {
    pb::FileInfo {
        path: f.path.clone(),
        size: f.size,
        is_dir: f.is_dir,
        mode: f.mode,
        modified_at: ts(f.modified_at),
    }
}

pub fn require_id(id: &str, what: &str) -> Result<(), Status> {
    if id.is_empty() {
        return Err(Status::invalid_argument(format!("{what} is required")));
    }
    Ok(())
}
