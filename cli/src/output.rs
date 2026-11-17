//! Tables and JSON.

use serde_json::{json, Value};
use vmlayer_proto::v1 as pb;

pub fn json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

fn table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for r in rows {
        for (i, cell) in r.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<w$}", c, w = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    println!("{}", line(headers.iter().map(|s| s.to_string()).collect()));
    for r in rows {
        println!("{}", line(r.clone()));
    }
}

fn state(s: i32) -> &'static str {
    match pb::SandboxState::try_from(s).unwrap_or(pb::SandboxState::Unspecified) {
        pb::SandboxState::Creating => "creating",
        pb::SandboxState::Running => "running",
        pb::SandboxState::Paused => "paused",
        pb::SandboxState::Stopped => "stopped",
        pb::SandboxState::Failed => "failed",
        pb::SandboxState::Unspecified => "?",
    }
}

fn age(t: &Option<prost_types::Timestamp>) -> String {
    let Some(t) = t else { return "-".into() };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let s = (now - t.seconds).max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

pub fn sandbox_json(s: &pb::Sandbox) -> Value {
    json!({
        "id": s.id, "backend": s.backend, "state": state(s.state), "image": s.image,
        "workdir": s.workdir, "workspace_volume_id": s.workspace_volume_id,
        "mounts": s.mounts.iter().map(|m| json!({"volume_id": m.volume_id, "guest_path": m.guest_path, "read_only": m.read_only})).collect::<Vec<_>>(),
        "exposed": s.exposed.iter().map(|(p, e)| (p.to_string(), Value::String(format!("{}:{}", e.host, e.port)))).collect::<serde_json::Map<_, _>>(),
        "labels": s.labels, "created_at": s.created_at.as_ref().map(|t| t.seconds),
    })
}

pub fn sandboxes(list: &[pb::Sandbox], as_json: bool) {
    if as_json {
        return json(&Value::Array(list.iter().map(sandbox_json).collect()));
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                s.backend.clone(),
                state(s.state).into(),
                s.image.clone(),
                age(&s.created_at),
                s.exposed
                    .iter()
                    .map(|(p, e)| format!("{p}->{}", e.port))
                    .collect::<Vec<_>>()
                    .join(","),
            ]
        })
        .collect();
    table(&["ID", "BACKEND", "STATE", "IMAGE", "AGE", "PORTS"], &rows);
}

pub fn volumes(list: &[pb::Volume], as_json: bool) {
    if as_json {
        let v: Vec<Value> = list
            .iter()
            .map(|v| json!({"id": v.id, "name": v.name, "size_mib": v.size_mib, "attached_to": v.attached_to, "anonymous": v.anonymous, "labels": v.labels}))
            .collect();
        return json(&Value::Array(v));
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|v| {
            vec![
                v.id.clone(),
                v.name.clone(),
                format!("{}M", v.size_mib),
                if v.anonymous {
                    "anon".into()
                } else {
                    "named".into()
                },
                v.attached_to.clone(),
                age(&v.created_at),
            ]
        })
        .collect();
    table(&["ID", "NAME", "SIZE", "KIND", "ATTACHED TO", "AGE"], &rows);
}

pub fn volume_snapshots(list: &[pb::VolumeSnapshot], as_json: bool) {
    if as_json {
        let v: Vec<Value> = list
            .iter()
            .map(|s| json!({"id": s.id, "volume_id": s.volume_id, "guest_path": s.guest_path, "size_bytes": s.size_bytes, "durable": s.durable, "local": s.local, "labels": s.labels}))
            .collect();
        return json(&Value::Array(v));
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                s.volume_id.clone(),
                s.guest_path.clone(),
                human(s.size_bytes),
                match (s.local, s.durable) {
                    (true, true) => "local+blob",
                    (true, false) => "local",
                    (false, _) => "blob",
                }
                .to_string(),
                age(&s.created_at),
            ]
        })
        .collect();
    table(
        &["ID", "VOLUME", "GUEST PATH", "SIZE", "STORED", "AGE"],
        &rows,
    );
}

pub fn snapshots(list: &[pb::Snapshot], as_json: bool) {
    if as_json {
        let v: Vec<Value> = list
            .iter()
            .map(|s| json!({"id": s.id, "backend": s.backend, "engine_version": s.engine_version, "includes_memory": s.includes_memory, "size_bytes": s.size_bytes, "volumes": s.volumes.len(), "labels": s.labels}))
            .collect();
        return json(&Value::Array(v));
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                s.backend.clone(),
                if s.includes_memory {
                    "mem+fs".into()
                } else {
                    "fs".into()
                },
                human(s.size_bytes),
                s.volumes.len().to_string(),
                age(&s.created_at),
            ]
        })
        .collect();
    table(&["ID", "BACKEND", "KIND", "SIZE", "VOLUMES", "AGE"], &rows);
}

pub fn info(i: &pb::SystemInfo, as_json: bool) {
    if as_json {
        let b: Vec<Value> = i
            .backends
            .iter()
            .map(|b| json!({"name": b.name, "engine_version": b.engine_version, "available": b.unavailable_reason.is_empty(), "unavailable_reason": b.unavailable_reason}))
            .collect();
        return json(
            &json!({"daemon_version": i.daemon_version, "default_backend": i.default_backend, "data_dir": i.data_dir, "blob_store": i.blob_store, "backends": b}),
        );
    }
    println!(
        "vmlayerd {}  default backend: {}  data dir: {}",
        i.daemon_version, i.default_backend, i.data_dir
    );
    if !i.blob_store.is_empty() {
        println!("blob store: {}", i.blob_store);
    }
    let rows: Vec<Vec<String>> = i
        .backends
        .iter()
        .map(|b| {
            let caps = b.capabilities.as_ref();
            let mut feats = Vec::new();
            if caps.map(|c| c.memory_snapshot).unwrap_or(false) {
                feats.push("memory-snapshot");
            } else if caps.map(|c| c.snapshot).unwrap_or(false) {
                feats.push("fs-snapshot");
            }
            if caps.map(|c| c.fork).unwrap_or(false) {
                feats.push("fork");
            }
            if caps.map(|c| c.pty).unwrap_or(false) {
                feats.push("pty");
            }
            vec![
                b.name.clone(),
                if b.unavailable_reason.is_empty() {
                    "ready".into()
                } else {
                    "unavailable".into()
                },
                b.engine_version.clone(),
                feats.join(","),
                b.unavailable_reason.clone(),
            ]
        })
        .collect();
    table(
        &["BACKEND", "STATUS", "ENGINE", "FEATURES", "REASON"],
        &rows,
    );
}

fn human(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}
