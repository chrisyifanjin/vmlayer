//! Durable per-host state in SQLite. One `Store`, blocking calls wrapped in
//! `spawn_blocking` by callers that are on the async runtime.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use vmlayer_core::{
    SandboxId, SandboxSpec, SandboxState, SnapshotId, SnapshotRef, VolumeId, VolumeSnapshotId,
};

const MIGRATIONS: &[&str] = &[
    // 1
    "CREATE TABLE sandboxes (
        id TEXT PRIMARY KEY,
        backend TEXT NOT NULL,
        image TEXT NOT NULL,
        workdir TEXT NOT NULL,
        workspace_volume_id TEXT,
        state TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        ttl_secs INTEGER,
        labels TEXT NOT NULL,
        spec TEXT NOT NULL
    );
    CREATE TABLE volumes (
        id TEXT PRIMARY KEY,
        name TEXT UNIQUE,
        size_mib INTEGER NOT NULL,
        labels TEXT NOT NULL,
        anonymous INTEGER NOT NULL,
        attached_to TEXT,
        created_at INTEGER NOT NULL,
        path TEXT NOT NULL
    );
    CREATE TABLE mounts (
        sandbox_id TEXT NOT NULL,
        volume_id TEXT NOT NULL,
        guest_path TEXT NOT NULL,
        read_only INTEGER NOT NULL,
        PRIMARY KEY (sandbox_id, guest_path)
    );
    CREATE TABLE volume_snapshots (
        id TEXT PRIMARY KEY,
        volume_id TEXT NOT NULL,
        guest_path TEXT,
        created_at INTEGER NOT NULL,
        size_bytes INTEGER NOT NULL,
        durable INTEGER NOT NULL,
        labels TEXT NOT NULL,
        path TEXT NOT NULL
    );
    CREATE TABLE snapshots (
        id TEXT PRIMARY KEY,
        backend TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        labels TEXT NOT NULL,
        reference TEXT NOT NULL
    );",
    // 2: ports published on the HTTPS gateway (decision 0021), by the
    // SHA-256 of their token; the token itself is never stored.
    "CREATE TABLE gateway_routes (
        sandbox_id TEXT NOT NULL,
        port INTEGER NOT NULL,
        token_sha256 BLOB NOT NULL,
        PRIMARY KEY (sandbox_id, port)
    );",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxRow {
    pub id: SandboxId,
    pub backend: String,
    pub image: String,
    pub workdir: String,
    pub workspace_volume_id: Option<VolumeId>,
    pub state: SandboxState,
    pub created_at: SystemTime,
    pub ttl: Option<Duration>,
    pub labels: BTreeMap<String, String>,
    pub spec: SandboxSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeRow {
    pub id: VolumeId,
    pub name: Option<String>,
    pub size_mib: u64,
    pub labels: BTreeMap<String, String>,
    pub anonymous: bool,
    pub attached_to: Option<SandboxId>,
    pub created_at: SystemTime,
    pub path: std::path::PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountRow {
    pub sandbox_id: SandboxId,
    pub volume_id: VolumeId,
    pub guest_path: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeSnapshotRow {
    pub id: VolumeSnapshotId,
    pub volume_id: VolumeId,
    pub guest_path: Option<String>,
    pub created_at: SystemTime,
    pub size_bytes: u64,
    pub durable: bool,
    pub labels: BTreeMap<String, String>,
    pub path: std::path::PathBuf,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found")]
    NotFound,
}

fn now_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn from_secs(s: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(s.max(0) as u64)
}

fn state_str(s: SandboxState) -> &'static str {
    match s {
        SandboxState::Creating => "creating",
        SandboxState::Running => "running",
        SandboxState::Paused => "paused",
        SandboxState::Stopped => "stopped",
        SandboxState::Failed => "failed",
    }
}

fn state_parse(s: &str) -> SandboxState {
    match s {
        "creating" => SandboxState::Creating,
        "running" => SandboxState::Running,
        "paused" => SandboxState::Paused,
        "stopped" => SandboxState::Stopped,
        _ => SandboxState::Failed,
    }
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let target = i as i64 + 1;
            if version < target {
                conn.execute_batch(sql)?;
                conn.pragma_update(None, "user_version", target)?;
            }
        }
        Ok(())
    }

    // -- sandboxes --------------------------------------------------------

    pub fn insert_sandbox(&self, row: &SandboxRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sandboxes (id, backend, image, workdir, workspace_volume_id, state, created_at, ttl_secs, labels, spec)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                row.id.0,
                row.backend,
                row.image,
                row.workdir,
                row.workspace_volume_id.as_ref().map(|v| v.0.clone()),
                state_str(row.state),
                now_secs(row.created_at),
                row.ttl.map(|d| d.as_secs() as i64),
                serde_json::to_string(&row.labels)?,
                serde_json::to_string(&row.spec)?,
            ],
        )?;
        Ok(())
    }

    pub fn set_sandbox_state(&self, id: &SandboxId, state: SandboxState) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sandboxes SET state = ?2 WHERE id = ?1",
            params![id.0, state_str(state)],
        )?;
        Ok(())
    }

    pub fn delete_sandbox(&self, id: &SandboxId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM mounts WHERE sandbox_id = ?1", params![id.0])?;
        conn.execute(
            "DELETE FROM gateway_routes WHERE sandbox_id = ?1",
            params![id.0],
        )?;
        conn.execute("DELETE FROM sandboxes WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    // -- gateway routes ---------------------------------------------------

    pub fn put_route(&self, sandbox: &SandboxId, port: u16, token_sha256: &[u8; 32]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO gateway_routes (sandbox_id, port, token_sha256) VALUES (?1, ?2, ?3)",
            params![sandbox.0, port, token_sha256.as_slice()],
        )?;
        Ok(())
    }

    /// The token hash of a published port, if it is published.
    pub fn route(&self, sandbox: &SandboxId, port: u16) -> Result<Option<[u8; 32]>> {
        let conn = self.conn.lock().unwrap();
        let hash: Option<Vec<u8>> = conn
            .query_row(
                "SELECT token_sha256 FROM gateway_routes WHERE sandbox_id = ?1 AND port = ?2",
                params![sandbox.0, port],
                |r| r.get(0),
            )
            .optional()?;
        Ok(hash.and_then(|h| h.try_into().ok()))
    }

    pub fn delete_route(&self, sandbox: &SandboxId, port: u16) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM gateway_routes WHERE sandbox_id = ?1 AND port = ?2",
            params![sandbox.0, port],
        )?;
        Ok(())
    }

    fn row_to_sandbox(r: &rusqlite::Row<'_>) -> rusqlite::Result<SandboxRow> {
        let labels: String = r.get(8)?;
        let spec: String = r.get(9)?;
        Ok(SandboxRow {
            id: SandboxId(r.get(0)?),
            backend: r.get(1)?,
            image: r.get(2)?,
            workdir: r.get(3)?,
            workspace_volume_id: r.get::<_, Option<String>>(4)?.map(VolumeId),
            state: state_parse(&r.get::<_, String>(5)?),
            created_at: from_secs(r.get(6)?),
            ttl: r
                .get::<_, Option<i64>>(7)?
                .map(|s| Duration::from_secs(s.max(0) as u64)),
            labels: serde_json::from_str(&labels).unwrap_or_default(),
            spec: serde_json::from_str(&spec).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
        })
    }

    const SANDBOX_COLS: &'static str =
        "id, backend, image, workdir, workspace_volume_id, state, created_at, ttl_secs, labels, spec";

    pub fn get_sandbox(&self, id: &SandboxId) -> Result<SandboxRow> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!("SELECT {} FROM sandboxes WHERE id = ?1", Self::SANDBOX_COLS),
            params![id.0],
            Self::row_to_sandbox,
        )
        .optional()?
        .ok_or(StoreError::NotFound)
    }

    pub fn list_sandboxes(&self) -> Result<Vec<SandboxRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM sandboxes ORDER BY created_at",
            Self::SANDBOX_COLS
        ))?;
        let rows = stmt.query_map([], Self::row_to_sandbox)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // -- volumes ------------------------------------------------------------

    pub fn insert_volume(&self, row: &VolumeRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO volumes (id, name, size_mib, labels, anonymous, attached_to, created_at, path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.id.0,
                row.name,
                row.size_mib as i64,
                serde_json::to_string(&row.labels)?,
                row.anonymous as i64,
                row.attached_to.as_ref().map(|s| s.0.clone()),
                now_secs(row.created_at),
                row.path.to_string_lossy().to_string(),
            ],
        )?;
        Ok(())
    }

    fn row_to_volume(r: &rusqlite::Row<'_>) -> rusqlite::Result<VolumeRow> {
        let labels: String = r.get(3)?;
        Ok(VolumeRow {
            id: VolumeId(r.get(0)?),
            name: r.get(1)?,
            size_mib: r.get::<_, i64>(2)?.max(0) as u64,
            labels: serde_json::from_str(&labels).unwrap_or_default(),
            anonymous: r.get::<_, i64>(4)? != 0,
            attached_to: r.get::<_, Option<String>>(5)?.map(SandboxId),
            created_at: from_secs(r.get(6)?),
            path: std::path::PathBuf::from(r.get::<_, String>(7)?),
        })
    }

    const VOLUME_COLS: &'static str =
        "id, name, size_mib, labels, anonymous, attached_to, created_at, path";

    /// Look up by id, then by name.
    pub fn get_volume(&self, id_or_name: &str) -> Result<VolumeRow> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT {} FROM volumes WHERE id = ?1 OR name = ?1 ORDER BY (id = ?1) DESC LIMIT 1",
                Self::VOLUME_COLS
            ),
            params![id_or_name],
            Self::row_to_volume,
        )
        .optional()?
        .ok_or(StoreError::NotFound)
    }

    pub fn list_volumes(&self) -> Result<Vec<VolumeRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM volumes ORDER BY created_at",
            Self::VOLUME_COLS
        ))?;
        let rows = stmt.query_map([], Self::row_to_volume)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_volume_attachment(
        &self,
        id: &VolumeId,
        attached_to: Option<&SandboxId>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE volumes SET attached_to = ?2 WHERE id = ?1",
            params![id.0, attached_to.map(|s| s.0.clone())],
        )?;
        Ok(())
    }

    pub fn delete_volume(&self, id: &VolumeId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM volumes WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    // -- mounts -------------------------------------------------------------

    pub fn insert_mount(&self, m: &MountRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mounts (sandbox_id, volume_id, guest_path, read_only) VALUES (?1, ?2, ?3, ?4)",
            params![m.sandbox_id.0, m.volume_id.0, m.guest_path, m.read_only as i64],
        )?;
        Ok(())
    }

    pub fn mounts_for(&self, sandbox: &SandboxId) -> Result<Vec<MountRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT sandbox_id, volume_id, guest_path, read_only FROM mounts WHERE sandbox_id = ?1 ORDER BY guest_path",
        )?;
        let rows = stmt.query_map(params![sandbox.0], |r| {
            Ok(MountRow {
                sandbox_id: SandboxId(r.get(0)?),
                volume_id: VolumeId(r.get(1)?),
                guest_path: r.get(2)?,
                read_only: r.get::<_, i64>(3)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // -- volume snapshots -----------------------------------------------------

    pub fn insert_volume_snapshot(&self, row: &VolumeSnapshotRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO volume_snapshots (id, volume_id, guest_path, created_at, size_bytes, durable, labels, path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.id.0,
                row.volume_id.0,
                row.guest_path,
                now_secs(row.created_at),
                row.size_bytes as i64,
                row.durable as i64,
                serde_json::to_string(&row.labels)?,
                row.path.to_string_lossy().to_string(),
            ],
        )?;
        Ok(())
    }

    fn row_to_volume_snapshot(r: &rusqlite::Row<'_>) -> rusqlite::Result<VolumeSnapshotRow> {
        let labels: String = r.get(6)?;
        Ok(VolumeSnapshotRow {
            id: VolumeSnapshotId(r.get(0)?),
            volume_id: VolumeId(r.get(1)?),
            guest_path: r.get(2)?,
            created_at: from_secs(r.get(3)?),
            size_bytes: r.get::<_, i64>(4)?.max(0) as u64,
            durable: r.get::<_, i64>(5)? != 0,
            labels: serde_json::from_str(&labels).unwrap_or_default(),
            path: std::path::PathBuf::from(r.get::<_, String>(7)?),
        })
    }

    const VSNAP_COLS: &'static str =
        "id, volume_id, guest_path, created_at, size_bytes, durable, labels, path";

    pub fn get_volume_snapshot(&self, id: &VolumeSnapshotId) -> Result<VolumeSnapshotRow> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT {} FROM volume_snapshots WHERE id = ?1",
                Self::VSNAP_COLS
            ),
            params![id.0],
            Self::row_to_volume_snapshot,
        )
        .optional()?
        .ok_or(StoreError::NotFound)
    }

    pub fn list_volume_snapshots(&self) -> Result<Vec<VolumeSnapshotRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM volume_snapshots ORDER BY created_at",
            Self::VSNAP_COLS
        ))?;
        let rows = stmt.query_map([], Self::row_to_volume_snapshot)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_volume_snapshot(&self, id: &VolumeSnapshotId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM volume_snapshots WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    // -- sandbox snapshots ----------------------------------------------------

    pub fn insert_snapshot(&self, s: &SnapshotRef) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO snapshots (id, backend, created_at, labels, reference) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                s.id.0,
                s.backend,
                now_secs(s.created_at),
                serde_json::to_string(&s.labels)?,
                serde_json::to_string(s)?,
            ],
        )?;
        Ok(())
    }

    pub fn get_snapshot(&self, id: &SnapshotId) -> Result<SnapshotRef> {
        let conn = self.conn.lock().unwrap();
        let text: Option<String> = conn
            .query_row(
                "SELECT reference FROM snapshots WHERE id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .optional()?;
        let text = text.ok_or(StoreError::NotFound)?;
        let parsed: SnapshotRef = serde_json::from_str(&text)?;
        Ok(parsed)
    }

    pub fn list_snapshots(&self) -> Result<Vec<SnapshotRef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT reference FROM snapshots ORDER BY created_at")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for text in rows {
            let text = text?;
            let parsed: SnapshotRef = serde_json::from_str(&text)?;
            out.push(parsed);
        }
        Ok(out)
    }

    pub fn delete_snapshot(&self, id: &SnapshotId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM snapshots WHERE id = ?1", params![id.0])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> SandboxSpec {
        SandboxSpec {
            id: SandboxId(id.into()),
            rootfs: vmlayer_core::Rootfs::EngineImage {
                digest: "alpine".into(),
            },
            mounts: vec![],
            vcpus: 1,
            memory_mib: 64,
            disk_mib: None,
            env: BTreeMap::new(),
            workdir: "/work".into(),
            entrypoint: None,
            network: vmlayer_core::NetworkMode::Egress,
            egress: None,
            limits: Default::default(),
            ttl: None,
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn sandbox_round_trip() {
        let s = Store::open_in_memory().unwrap();
        let row = SandboxRow {
            id: SandboxId("sb1".into()),
            backend: "docker".into(),
            image: "alpine".into(),
            workdir: "/work".into(),
            workspace_volume_id: Some(VolumeId("v1".into())),
            state: SandboxState::Running,
            created_at: SystemTime::now(),
            ttl: Some(Duration::from_secs(60)),
            labels: BTreeMap::from([("a".into(), "b".into())]),
            spec: spec("sb1"),
        };
        s.insert_sandbox(&row).unwrap();
        let got = s.get_sandbox(&row.id).unwrap();
        assert_eq!(got.backend, "docker");
        assert_eq!(got.ttl, Some(Duration::from_secs(60)));
        assert_eq!(got.labels["a"], "b");
        assert_eq!(got.spec.memory_mib, 64);
        s.set_sandbox_state(&row.id, SandboxState::Stopped).unwrap();
        assert_eq!(s.get_sandbox(&row.id).unwrap().state, SandboxState::Stopped);
        assert_eq!(s.list_sandboxes().unwrap().len(), 1);
        s.delete_sandbox(&row.id).unwrap();
        assert!(matches!(s.get_sandbox(&row.id), Err(StoreError::NotFound)));
    }

    #[test]
    fn volume_by_name_or_id() {
        let s = Store::open_in_memory().unwrap();
        let v = VolumeRow {
            id: VolumeId("vol-1".into()),
            name: Some("proj".into()),
            size_mib: 100,
            labels: BTreeMap::new(),
            anonymous: false,
            attached_to: None,
            created_at: SystemTime::now(),
            path: "/tmp/v".into(),
        };
        s.insert_volume(&v).unwrap();
        assert_eq!(s.get_volume("vol-1").unwrap().id, v.id);
        assert_eq!(s.get_volume("proj").unwrap().id, v.id);
        s.set_volume_attachment(&v.id, Some(&SandboxId("sb".into())))
            .unwrap();
        assert_eq!(
            s.get_volume("proj").unwrap().attached_to,
            Some(SandboxId("sb".into()))
        );
        // unique names
        let dup = VolumeRow {
            id: VolumeId("vol-2".into()),
            ..v.clone()
        };
        assert!(s.insert_volume(&dup).is_err());
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        Store::open(&p).unwrap();
        Store::open(&p).unwrap();
    }
}
