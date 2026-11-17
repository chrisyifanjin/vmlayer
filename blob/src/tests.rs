use std::collections::BTreeMap;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use object_store::memory::InMemory;
use object_store::{ObjectStore, ObjectStoreExt};

use super::*;

/// Deterministic pseudo-random bytes (xorshift64*).
pub(crate) fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

fn meta(id: &str) -> SnapshotMeta {
    SnapshotMeta {
        id: id.into(),
        volume_id: "vol-1".into(),
        size_mib: 20,
        guest_path: Some("/work".into()),
        labels: BTreeMap::from([("k".to_string(), "v".to_string())]),
        created_at: 1,
    }
}

fn memory() -> (Remote, Arc<InMemory>) {
    let store = Arc::new(InMemory::new());
    (Remote::from_store(store.clone(), "pfx"), store)
}

fn sparse_image(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("image.ext4");
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(20 << 20).unwrap();
    f.write_all_at(&noise(1, 5000), 10).unwrap();
    // Straddles the 4 MiB boundary between the second and third chunk.
    f.write_all_at(&noise(2, 3 << 20), (8 << 20) - (1 << 20))
        .unwrap();
    // The last, partial-size region of a file whose size is not aligned.
    f.set_len((20 << 20) + 123).unwrap();
    f.write_all_at(b"tail", 20 << 20).unwrap();
    path
}

#[tokio::test]
async fn block_image_round_trip_keeps_holes() {
    let (remote, _) = memory();
    let dir = tempfile::tempdir().unwrap();
    let image = sparse_image(dir.path());
    let (manifest, stats) = remote.push_block(&image, &meta("vsnap-a")).await.unwrap();
    // Chunks 0, 1, 2 and the tail chunk carry data; the zero ones are omitted.
    assert_eq!(stats.chunks, 4);
    assert_eq!(stats.uploaded_chunks, 4);
    assert_eq!(manifest.logical_size, (20 << 20) + 123);
    assert_eq!(remote.manifest("vsnap-a").await.unwrap(), manifest);

    let out = dir.path().join("restored.ext4");
    remote.pull(&manifest, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&image).unwrap());
}

#[tokio::test]
async fn unchanged_blocks_are_not_uploaded_again() {
    let (remote, _) = memory();
    let dir = tempfile::tempdir().unwrap();
    let image = sparse_image(dir.path());
    remote.push_block(&image, &meta("vsnap-a")).await.unwrap();
    let (_, again) = remote.push_block(&image, &meta("vsnap-b")).await.unwrap();
    assert_eq!(
        (again.chunks, again.uploaded_chunks, again.uploaded_bytes),
        (4, 0, 0)
    );

    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap();
    f.write_all_at(b"changed", 13 << 20).unwrap();
    let (_, delta) = remote.push_block(&image, &meta("vsnap-c")).await.unwrap();
    assert_eq!((delta.chunks, delta.uploaded_chunks), (5, 1));
}

fn sample_tree(root: &Path) {
    std::fs::create_dir_all(root.join("src/deep/er")).unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("README"), b"hello\n").unwrap();
    std::fs::write(root.join("src/main.py"), b"print(42)\n").unwrap();
    std::fs::write(root.join("src/deep/er/big.bin"), noise(3, 9 << 20)).unwrap();
    std::fs::write(root.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("src/main.py", root.join("link")).unwrap();
}

/// (relative path, kind, mode, content or link target) for every entry.
fn listing(root: &Path) -> Vec<(String, &'static str, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let md = std::fs::symlink_metadata(&path).unwrap();
            let mode = md.permissions().mode() & 0o7777;
            if md.is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.push((rel, "link", 0, target.display().to_string().into_bytes()));
            } else if md.is_dir() {
                out.push((rel, "dir", mode, Vec::new()));
                stack.push(path);
            } else {
                out.push((rel, "file", mode, std::fs::read(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn directory_round_trip() {
    let (remote, _) = memory();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    sample_tree(&src);
    let (manifest, stats) = remote.push_dir(&src, &meta("vsnap-d")).await.unwrap();
    assert_eq!(manifest.kind, Kind::Dir);
    assert!(
        stats.chunks >= 2,
        "9 MiB of data should span several chunks"
    );

    let dst = dir.path().join("dst");
    std::fs::create_dir(&dst).unwrap();
    remote.pull(&manifest, &dst).await.unwrap();
    assert_eq!(listing(&dst), listing(&src));

    let (_, again) = remote.push_dir(&src, &meta("vsnap-e")).await.unwrap();
    assert_eq!(
        again.uploaded_chunks, 0,
        "an unchanged tree uploads nothing"
    );
}

#[tokio::test]
async fn corrupt_chunks_are_rejected() {
    let (remote, store) = memory();
    let dir = tempfile::tempdir().unwrap();
    let image = sparse_image(dir.path());
    let (manifest, _) = remote.push_block(&image, &meta("vsnap-a")).await.unwrap();
    let victim = remote.key(&chunk::chunk_rel(&manifest.chunks[1].hash));
    let bogus = zstd::bulk::compress(&vec![7u8; manifest.chunks[1].length as usize], 3).unwrap();
    store.put(&victim, bogus.into()).await.unwrap();
    let err = remote
        .pull(&manifest, &dir.path().join("out.ext4"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err}");

    store.delete(&victim).await.unwrap();
    let err = remote
        .pull(&manifest, &dir.path().join("out2.ext4"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("missing"), "{err}");
}

#[tokio::test]
async fn gc_removes_only_unreferenced_old_chunks() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let remote = Remote::from_store(store, "").with_gc_grace(Duration::ZERO);
    let dir = tempfile::tempdir().unwrap();
    let image = sparse_image(dir.path());
    let (a, _) = remote.push_block(&image, &meta("vsnap-a")).await.unwrap();
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap();
    f.write_all_at(b"only in b", 13 << 20).unwrap();
    let (b, _) = remote.push_block(&image, &meta("vsnap-b")).await.unwrap();

    remote.delete("vsnap-a").await.unwrap();
    assert!(matches!(
        remote.manifest("vsnap-a").await,
        Err(Error::NotFound(_))
    ));
    let dry = remote.gc(true).await.unwrap();
    assert_eq!((dry.manifests, dry.deleted), (1, 0), "{dry:?}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let dry = remote.gc(true).await.unwrap();
    assert_eq!(dry.deleted, 0, "every chunk of a is shared with b: {dry:?}");

    // c differs from b in one block, so b's version of it becomes garbage.
    remote.delete("vsnap-b").await.unwrap();
    f.write_all_at(b"only in c", 13 << 20).unwrap();
    let (_, c) = remote.push_block(&image, &meta("vsnap-c")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let real = remote.gc(false).await.unwrap();
    assert_eq!(real.deleted, 1, "{real:?}");
    assert_eq!(real.chunks, c.chunks + 1);
    let out = dir.path().join("c.ext4");
    remote
        .pull(&remote.manifest("vsnap-c").await.unwrap(), &out)
        .await
        .unwrap();
    assert_eq!(std::fs::read(out).unwrap(), std::fs::read(&image).unwrap());
    assert_eq!(a.chunks.len(), b.chunks.len() - 1);
}

#[tokio::test]
async fn list_and_validation() {
    let (remote, store) = memory();
    let dir = tempfile::tempdir().unwrap();
    let image = sparse_image(dir.path());
    let mut second = meta("vsnap-2");
    second.created_at = 2;
    remote.push_block(&image, &second).await.unwrap();
    remote.push_block(&image, &meta("vsnap-1")).await.unwrap();
    let ids: Vec<_> = remote
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["vsnap-1", "vsnap-2"]);

    assert!(remote.manifest("../etc/passwd").await.is_err());
    let bad = br#"{"format":"other/9"}"#;
    store
        .put(
            &remote.key("volume-snapshots/vsnap-x.json"),
            bad.to_vec().into(),
        )
        .await
        .unwrap();
    assert!(matches!(
        remote.manifest("vsnap-x").await,
        Err(Error::Manifest(_))
    ));
    assert!(
        remote.gc(true).await.is_err(),
        "gc must stop on an unreadable manifest"
    );
}

#[test]
fn open_parses_urls_and_hides_credentials() {
    let local = Remote::open("file:///tmp/vmlayer-blob-test", &BTreeMap::new()).unwrap();
    assert_eq!(local.describe(), "file:///tmp/vmlayer-blob-test");
    let s3 = Remote::open(
        "s3://key:secret@bucket/some/prefix?x=1",
        &BTreeMap::from([("region".to_string(), "eu-west-1".to_string())]),
    )
    .unwrap();
    assert_eq!(s3.describe(), "s3://bucket/some/prefix");
    assert_eq!(s3.prefix, "some/prefix");
    assert!(Remote::open("not a url", &BTreeMap::new()).is_err());
}

#[tokio::test]
async fn sandbox_snapshot_files_live_apart_and_survive_gc() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let remote = Remote::from_store(store, "").with_gc_grace(Duration::ZERO);
    let dir = tempfile::tempdir().unwrap();
    let image = sparse_image(dir.path());
    let files = remote.files_of("snap-1").unwrap();
    files.push_block(&image, &meta("mem")).await.unwrap();
    // Not a volume snapshot...
    assert!(remote.list().await.unwrap().is_empty());
    assert_eq!(files.list().await.unwrap().len(), 1);
    // ...but its chunks are referenced.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(remote.gc(false).await.unwrap().deleted, 0);
    let out = dir.path().join("mem.out");
    files
        .pull(&files.manifest("mem").await.unwrap(), &out)
        .await
        .unwrap();
    assert_eq!(std::fs::read(out).unwrap(), std::fs::read(&image).unwrap());
    assert!(remote.files_of("../x").is_err());
}
