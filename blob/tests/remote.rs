//! The blob store against real object storage APIs. Skipped unless the
//! endpoints are given; `scripts/blob-test-stores.sh start` provides MinIO
//! (S3) and Azurite (Azure Blob) and prints the variables:
//!
//!   VMLAYER_TEST_S3_URL     e.g. s3://bucket/prefix, plus AWS_* settings
//!   VMLAYER_TEST_AZURE_URL  e.g. az://container/prefix, plus AZURE_* settings

use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;
use std::time::{SystemTime, UNIX_EPOCH};

use vmlayer_blob::{Kind, Remote, SnapshotMeta};

fn noise(seed: u64, len: usize) -> Vec<u8> {
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
        volume_id: "vol-it".into(),
        size_mib: 32,
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        ..Default::default()
    }
}

async fn exercise(url: &str) {
    // A fresh prefix per run keeps runs independent in a shared bucket.
    let run = format!("{url}/{}", std::process::id());
    let remote = Remote::open(&run, &BTreeMap::new()).expect("open store");
    let dir = tempfile::tempdir().unwrap();

    let image = dir.path().join("image.ext4");
    let f = std::fs::File::create(&image).unwrap();
    f.set_len(32 << 20).unwrap();
    f.write_all_at(&noise(1, 6 << 20), 1 << 20).unwrap();
    let (m, stats) = remote
        .push_block(&image, &meta("vsnap-it-block"))
        .await
        .unwrap();
    assert_eq!(stats.uploaded_chunks, stats.chunks);
    let (_, again) = remote
        .push_block(&image, &meta("vsnap-it-block2"))
        .await
        .unwrap();
    assert_eq!(
        again.uploaded_chunks, 0,
        "{url}: unchanged blocks were uploaded again"
    );
    let out = dir.path().join("out.ext4");
    remote.pull(&m, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&image).unwrap());

    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("a/b")).unwrap();
    std::fs::write(tree.join("a/b/data"), noise(2, 5 << 20)).unwrap();
    std::fs::write(tree.join("hello"), b"hi").unwrap();
    let (d, _) = remote.push_dir(&tree, &meta("vsnap-it-dir")).await.unwrap();
    assert_eq!(d.kind, Kind::Dir);
    let back = dir.path().join("back");
    std::fs::create_dir(&back).unwrap();
    remote.pull(&d, &back).await.unwrap();
    assert_eq!(
        std::fs::read(back.join("a/b/data")).unwrap(),
        std::fs::read(tree.join("a/b/data")).unwrap()
    );

    let ids: Vec<_> = remote
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    let gc = remote.gc(true).await.unwrap();
    assert_eq!((gc.manifests, gc.deleted), (3, 0), "{gc:?}");
    for id in ids {
        remote.delete(&id).await.unwrap();
    }
    assert!(remote.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn s3() {
    match std::env::var("VMLAYER_TEST_S3_URL") {
        Ok(url) => exercise(&url).await,
        Err(_) => eprintln!("VMLAYER_TEST_S3_URL not set; skipping"),
    }
}

#[tokio::test]
async fn azure_blob() {
    match std::env::var("VMLAYER_TEST_AZURE_URL") {
        Ok(url) => exercise(&url).await,
        Err(_) => eprintln!("VMLAYER_TEST_AZURE_URL not set; skipping"),
    }
}
