//! Leases: at most one holder of a name across every daemon sharing a store
//! (decision 0014).
//!
//! A lease is a small JSON object at `<prefix>/leases/<name>.json`. It is
//! taken with a create-if-absent write and renewed or taken over with a
//! write that succeeds only if the object is unchanged since it was read
//! (`If-None-Match` / `If-Match` on S3, Azure and GCS; `PutMode::Create` and
//! `PutMode::Update` in `object_store`). An expired lease can be taken over by
//! anyone, so a crashed holder blocks its name for at most one TTL.
//!
//! A `file://` directory has no conditional updates and falls back to
//! delete-then-create for takeover and a checked overwrite for renewal. That
//! is safe against crashes but not against two takers racing on an expired
//! lease within milliseconds; use object storage for multi-host deployments.
//! Any other store that cannot do a conditional update fails the write
//! instead of silently losing the guarantee.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};

use crate::{manifest, Error, Remote, Result};

const LEASE_DIR: &str = "leases";

impl Remote {
    fn no_cas(&self) -> Error {
        Error::Config(format!(
            "{}: this store does not support conditional writes, which leases and \
             ownership records need (on S3-compatible stores, check that the server \
             supports If-Match and that aws_conditional_put is not disabled)",
            self.display
        ))
    }
}

/// An opaque object version for [`Remote::put_if`].
#[derive(Clone, Debug)]
pub struct Version(UpdateVersion);

/// The stored record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub name: String,
    /// Who holds it, e.g. `<host>/<sandbox id>`.
    pub holder: String,
    /// Milliseconds since the Unix epoch.
    pub expires_at: u64,
}

impl LeaseRecord {
    fn expired(&self) -> bool {
        self.expires_at <= now_ms()
    }
}

/// Why a lease could not be taken or kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// Someone else holds it until `expires_at`.
    Held { holder: String, expires_at: u64 },
    /// It expired and was taken by someone else.
    Lost { holder: String },
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held { holder, expires_at } => {
                let secs = expires_at.saturating_sub(now_ms()).div_ceil(1000);
                write!(f, "held by {holder} for up to {secs} more seconds")
            }
            Self::Lost { holder } => write!(f, "lost to {holder}"),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

enum Put {
    Absent,
    Version(UpdateVersion),
}

impl Remote {
    fn lease_key(&self, name: &str) -> Result<object_store::path::Path> {
        manifest::check_id(name)?;
        Ok(self.key(&format!("{LEASE_DIR}/{name}.json")))
    }

    async fn read_lease(&self, name: &str) -> Result<Option<(LeaseRecord, UpdateVersion)>> {
        let key = self.lease_key(name)?;
        match self.store.get(&key).await {
            Ok(r) => {
                let version = UpdateVersion {
                    e_tag: r.meta.e_tag.clone(),
                    version: r.meta.version.clone(),
                };
                let bytes = r.bytes().await?;
                let record = serde_json::from_slice(&bytes)
                    .map_err(|e| Error::Manifest(format!("lease {name}: {e}")))?;
                Ok(Some((record, version)))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write `record`; `Ok(false)` if someone else wrote first.
    async fn write_lease(&self, record: &LeaseRecord, how: Put) -> Result<bool> {
        let key = self.lease_key(&record.name)?;
        let body = PutPayload::from(
            serde_json::to_vec(record).map_err(|e| Error::Manifest(e.to_string()))?,
        );
        let mode = match how {
            Put::Absent => PutMode::Create,
            Put::Version(v) => PutMode::Update(v),
        };
        match self
            .store
            .put_opts(&key, body.clone(), PutOptions::from(mode.clone()))
            .await
        {
            Ok(_) => Ok(true),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => Ok(false),
            Err(object_store::Error::NotImplemented { .. })
                if matches!(mode, PutMode::Update(_)) && self.local_dir =>
            {
                // No compare-and-swap here: replace by delete-then-create.
                match self.store.delete(&key).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
                Box::pin(self.write_lease(record, Put::Absent)).await
            }
            Err(object_store::Error::NotImplemented { .. }) => Err(self.no_cas()),
            Err(e) => Err(e.into()),
        }
    }

    /// Take `name` for `holder` for `ttl`, or renew it if `holder` has it.
    pub async fn acquire_lease(
        &self,
        name: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<std::result::Result<LeaseRecord, LeaseError>> {
        let record = LeaseRecord {
            name: name.to_string(),
            holder: holder.to_string(),
            expires_at: now_ms() + ttl.as_millis() as u64,
        };
        for _ in 0..4 {
            match self.read_lease(name).await? {
                None => {
                    if self.write_lease(&record, Put::Absent).await? {
                        return Ok(Ok(record));
                    }
                }
                Some((current, version)) => {
                    if current.holder != holder && !current.expired() {
                        return Ok(Err(LeaseError::Held {
                            holder: current.holder,
                            expires_at: current.expires_at,
                        }));
                    }
                    if self.write_lease(&record, Put::Version(version)).await? {
                        return Ok(Ok(record));
                    }
                }
            }
            // Lost a race: look again.
        }
        match self.read_lease(name).await? {
            Some((current, _)) => Ok(Err(LeaseError::Held {
                holder: current.holder,
                expires_at: current.expires_at,
            })),
            None => Err(Error::Manifest(format!(
                "lease {name}: could not be written"
            ))),
        }
    }

    /// Extend a lease `holder` has. Fails with `Lost` if someone else has it.
    pub async fn renew_lease(
        &self,
        name: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<std::result::Result<LeaseRecord, LeaseError>> {
        let Some((current, version)) = self.read_lease(name).await? else {
            // Deleted under us (e.g. by a takeover fallback): take it back if free.
            return self.acquire_lease(name, holder, ttl).await;
        };
        if current.holder != holder {
            return Ok(Err(LeaseError::Lost {
                holder: current.holder,
            }));
        }
        let record = LeaseRecord {
            expires_at: now_ms() + ttl.as_millis() as u64,
            ..current
        };
        if self.write_lease(&record, Put::Version(version)).await? {
            Ok(Ok(record))
        } else {
            let holder = self
                .read_lease(name)
                .await?
                .map(|(r, _)| r.holder)
                .unwrap_or_default();
            Ok(Err(LeaseError::Lost { holder }))
        }
    }

    /// Give up a lease if `holder` has it.
    pub async fn release_lease(&self, name: &str, holder: &str) -> Result<()> {
        if let Some((current, _)) = self.read_lease(name).await? {
            if current.holder == holder {
                match self.store.delete(&self.lease_key(name)?).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(())
    }

    /// The current record, if any (expired or not).
    pub async fn lease(&self, name: &str) -> Result<Option<LeaseRecord>> {
        Ok(self.read_lease(name).await?.map(|(r, _)| r))
    }

    /// Store `doc` under `<prefix>/<dir>/<name>.json` (host heartbeats).
    pub async fn put_doc(&self, dir: &str, name: &str, doc: &[u8]) -> Result<()> {
        manifest::check_id(name)?;
        self.store
            .put(
                &self.key(&format!("{dir}/{name}.json")),
                doc.to_vec().into(),
            )
            .await?;
        Ok(())
    }

    /// A document and the version to compare against when replacing it.
    pub async fn get_versioned(&self, dir: &str, name: &str) -> Result<Option<(Vec<u8>, Version)>> {
        manifest::check_id(name)?;
        match self
            .store
            .get(&self.key(&format!("{dir}/{name}.json")))
            .await
        {
            Ok(r) => {
                let version = Version(UpdateVersion {
                    e_tag: r.meta.e_tag.clone(),
                    version: r.meta.version.clone(),
                });
                Ok(Some((r.bytes().await?.to_vec(), version)))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write a document only if it is unchanged: `expected` is `None` for
    /// "must not exist yet", or the version read with [`Remote::get_versioned`].
    /// `Ok(false)` means someone else wrote first.
    pub async fn put_if(
        &self,
        dir: &str,
        name: &str,
        doc: &[u8],
        expected: Option<Version>,
    ) -> Result<bool> {
        manifest::check_id(name)?;
        let key = self.key(&format!("{dir}/{name}.json"));
        let mode = match expected {
            None => PutMode::Create,
            Some(Version(v)) => PutMode::Update(v),
        };
        let payload = PutPayload::from(doc.to_vec());
        match self
            .store
            .put_opts(&key, payload.clone(), PutOptions::from(mode.clone()))
            .await
        {
            Ok(_) => Ok(true),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => Ok(false),
            // No compare-and-swap (a directory store): best effort, see the
            // module docs.
            Err(object_store::Error::NotImplemented { .. })
                if matches!(mode, PutMode::Update(_)) && self.local_dir =>
            {
                match self.store.delete(&key).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
                match self
                    .store
                    .put_opts(&key, payload, PutOptions::from(PutMode::Create))
                    .await
                {
                    Ok(_) => Ok(true),
                    Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
                    Err(e) => Err(e.into()),
                }
            }
            Err(object_store::Error::NotImplemented { .. }) => Err(self.no_cas()),
            Err(e) => Err(e.into()),
        }
    }

    /// The document `<prefix>/<dir>/<name>.json`, if it exists.
    pub async fn get_doc(&self, dir: &str, name: &str) -> Result<Option<Vec<u8>>> {
        manifest::check_id(name)?;
        match self
            .store
            .get(&self.key(&format!("{dir}/{name}.json")))
            .await
        {
            Ok(r) => Ok(Some(r.bytes().await?.to_vec())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete_doc(&self, dir: &str, name: &str) -> Result<()> {
        manifest::check_id(name)?;
        match self
            .store
            .delete(&self.key(&format!("{dir}/{name}.json")))
            .await
        {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Every document under `<prefix>/<dir>/`.
    pub async fn list_docs(&self, dir: &str) -> Result<Vec<Vec<u8>>> {
        use futures::TryStreamExt;
        let prefix = self.key(dir);
        let metas: Vec<_> = match self.store.list(Some(&prefix)).try_collect().await {
            Ok(m) => m,
            Err(object_store::Error::NotFound { .. }) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for meta in metas {
            match self.store.get(&meta.location).await {
                Ok(r) => out.push(r.bytes().await?.to_vec()),
                Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    async fn exercise(remote: Remote) {
        let ttl = Duration::from_millis(400);
        let a = remote
            .acquire_lease("repo-42", "host-a/sb1", ttl)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.holder, "host-a/sb1");
        // Someone else is refused while it is live.
        let held = remote
            .acquire_lease("repo-42", "host-b/sb2", ttl)
            .await
            .unwrap();
        assert!(matches!(held, Err(LeaseError::Held { ref holder, .. }) if holder == "host-a/sb1"));
        // The holder renews; re-acquiring as the holder also renews.
        remote
            .renew_lease("repo-42", "host-a/sb1", ttl)
            .await
            .unwrap()
            .unwrap();
        remote
            .acquire_lease("repo-42", "host-a/sb1", ttl)
            .await
            .unwrap()
            .unwrap();
        // After expiry another holder takes over, and the old one has lost it.
        tokio::time::sleep(Duration::from_millis(500)).await;
        remote
            .acquire_lease("repo-42", "host-b/sb2", ttl)
            .await
            .unwrap()
            .unwrap();
        let lost = remote
            .renew_lease("repo-42", "host-a/sb1", ttl)
            .await
            .unwrap();
        assert!(matches!(lost, Err(LeaseError::Lost { ref holder }) if holder == "host-b/sb2"));
        // Only the holder's release counts.
        remote.release_lease("repo-42", "host-a/sb1").await.unwrap();
        assert!(remote.lease("repo-42").await.unwrap().is_some());
        remote.release_lease("repo-42", "host-b/sb2").await.unwrap();
        assert!(remote.lease("repo-42").await.unwrap().is_none());
        assert!(remote.acquire_lease("../x", "h", ttl).await.is_err());
    }

    #[tokio::test]
    async fn leases_with_compare_and_swap() {
        exercise(Remote::from_store(Arc::new(InMemory::new()), "p")).await;
    }

    #[tokio::test]
    async fn leases_on_a_directory_store() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().display());
        exercise(Remote::open(&url, &Default::default()).unwrap()).await;
    }

    #[tokio::test]
    async fn concurrent_takers_get_one_winner() {
        let remote = Remote::from_store(Arc::new(InMemory::new()), "");
        let ttl = Duration::from_secs(30);
        let results = futures::future::join_all((0..16).map(|i| {
            let r = remote.clone();
            async move { r.acquire_lease("one", &format!("h{i}"), ttl).await.unwrap() }
        }))
        .await;
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    }

    #[tokio::test]
    async fn documents() {
        let remote = Remote::from_store(Arc::new(InMemory::new()), "p");
        remote.put_doc("hosts", "h1", b"{}").await.unwrap();
        remote.put_doc("hosts", "h2", b"[]").await.unwrap();
        assert_eq!(remote.list_docs("hosts").await.unwrap().len(), 2);
        remote.delete_doc("hosts", "h1").await.unwrap();
        assert_eq!(
            remote.list_docs("hosts").await.unwrap(),
            vec![b"[]".to_vec()]
        );
    }
}

#[cfg(test)]
mod versioned_tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn put_if_is_compare_and_swap() {
        let r = Remote::from_store(Arc::new(InMemory::new()), "");
        assert!(r.put_if("owners", "x", b"1", None).await.unwrap());
        assert!(
            !r.put_if("owners", "x", b"2", None).await.unwrap(),
            "create must not overwrite"
        );
        let (_, v1) = r.get_versioned("owners", "x").await.unwrap().unwrap();
        assert!(r
            .put_if("owners", "x", b"2", Some(v1.clone()))
            .await
            .unwrap());
        assert!(
            !r.put_if("owners", "x", b"3", Some(v1)).await.unwrap(),
            "stale version must lose"
        );
        assert_eq!(
            r.get_versioned("owners", "x").await.unwrap().unwrap().0,
            b"2"
        );
    }
}
