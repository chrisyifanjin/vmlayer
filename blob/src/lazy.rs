//! A block image restored from the blob store on demand (decision 0016).
//!
//! The image starts as a sparse local file the size of the snapshot. A read
//! or write touching a chunk that is not local yet downloads that chunk
//! first; chunks the snapshot does not store are zeros and count as local
//! from the start. A one-byte-per-chunk marker file next to the image records
//! which chunks are local, so a daemon restart resumes where it was.
//! `hydrate` downloads everything still missing, in the background, after
//! which the image is an ordinary file.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use tokio::sync::Mutex;

use crate::{ChunkRef, Chunking, Error, Manifest, Remote, Result};

pub struct LazyImage {
    remote: Remote,
    image: Arc<File>,
    present_file: Arc<File>,
    present: std::sync::Mutex<Vec<bool>>,
    missing: AtomicUsize,
    chunk: u64,
    size: u64,
    chunks: HashMap<u64, ChunkRef>,
    locks: Vec<Mutex<()>>,
    present_path: PathBuf,
}

impl std::fmt::Debug for LazyImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyImage")
            .field("size", &self.size)
            .field("missing", &self.missing())
            .finish()
    }
}

/// Marker file kept next to the image while it is incomplete.
pub fn present_path(image: &Path) -> PathBuf {
    let mut p = image.as_os_str().to_owned();
    p.push(".present");
    PathBuf::from(p)
}

impl LazyImage {
    /// Start (or resume, if the marker file exists) a lazy image at `image`.
    pub fn open(remote: Remote, manifest: &Manifest, image: &Path) -> Result<Arc<Self>> {
        let Chunking::Fixed { size: chunk } = manifest.chunking else {
            return Err(Error::Manifest(
                "only block-image snapshots can be restored lazily".into(),
            ));
        };
        let count = manifest.logical_size.div_ceil(chunk) as usize;
        let chunks: HashMap<u64, ChunkRef> = manifest
            .chunks
            .iter()
            .map(|c| (c.offset / chunk, c.clone()))
            .collect();
        let marker = present_path(image);
        let resuming = marker.exists() && image.exists();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(image)?;
        let present_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&marker)?;
        let mut present = vec![false; count];
        if resuming {
            let mut bytes = vec![0u8; count];
            let n = present_file.read_at(&mut bytes, 0)?;
            for (i, b) in bytes[..n].iter().enumerate() {
                present[i] = *b == 1;
            }
        } else {
            file.set_len(manifest.logical_size)?;
            present_file.set_len(count as u64)?;
        }
        // Chunks the snapshot does not store are zeros: the sparse file has them.
        for (i, p) in present.iter_mut().enumerate() {
            if !chunks.contains_key(&(i as u64)) {
                *p = true;
            }
        }
        let missing = present.iter().filter(|p| !**p).count();
        Ok(Arc::new(Self {
            remote,
            image: Arc::new(file),
            present_file: Arc::new(present_file),
            present: std::sync::Mutex::new(present),
            missing: AtomicUsize::new(missing),
            chunk,
            size: manifest.logical_size,
            chunks,
            locks: (0..count).map(|_| Mutex::new(())).collect(),
            present_path: marker,
        }))
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Chunks not downloaded yet.
    pub fn missing(&self) -> usize {
        self.missing.load(Ordering::Relaxed)
    }

    pub fn is_complete(&self) -> bool {
        self.missing() == 0
    }

    fn is_present(&self, i: usize) -> bool {
        self.present.lock().unwrap()[i]
    }

    fn mark(&self, i: usize) -> Result<()> {
        self.present_file.write_all_at(&[1], i as u64)?;
        let mut present = self.present.lock().unwrap();
        if !present[i] {
            present[i] = true;
            self.missing.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Make chunk `i` local; `overwritten` skips the download because the
    /// caller is about to replace the whole chunk.
    async fn ensure(&self, i: usize, overwritten: bool) -> Result<()> {
        if self.is_present(i) {
            return Ok(());
        }
        let _guard = self.locks[i].lock().await;
        if self.is_present(i) {
            return Ok(());
        }
        if !overwritten {
            let c = &self.chunks[&(i as u64)];
            let data = self.remote.get_chunk(c).await?;
            let (image, offset) = (self.image.clone(), c.offset);
            tokio::task::spawn_blocking(move || image.write_all_at(&data, offset))
                .await
                .map_err(std::io::Error::other)??;
        }
        self.mark(i)
    }

    fn span(&self, offset: u64, len: u64) -> std::ops::Range<usize> {
        let end = (offset + len).min(self.size);
        (offset / self.chunk) as usize..end.div_ceil(self.chunk) as usize
    }

    pub async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if offset + len as u64 > self.size {
            return Err(Error::Io(std::io::Error::other(
                "read past the end of the image",
            )));
        }
        for i in self.span(offset, len as u64) {
            self.ensure(i, false).await?;
        }
        let image = self.image.clone();
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            image.read_exact_at(&mut buf, offset).map(|_| buf)
        })
        .await
        .map_err(std::io::Error::other)?
        .map_err(Error::from)
    }

    pub async fn write(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        let end = offset + data.len() as u64;
        if end > self.size {
            return Err(Error::Io(std::io::Error::other(
                "write past the end of the image",
            )));
        }
        for i in self.span(offset, data.len() as u64) {
            let (start, stop) = (
                i as u64 * self.chunk,
                ((i as u64 + 1) * self.chunk).min(self.size),
            );
            // A chunk the write covers completely needs no download.
            let _guard = if offset <= start && end >= stop && !self.is_present(i) {
                let guard = self.locks[i].lock().await;
                Some(guard)
            } else {
                self.ensure(i, false).await?;
                None
            };
            if _guard.is_some() && !self.is_present(i) {
                let chunk_data = data[(start - offset) as usize..(stop - offset) as usize].to_vec();
                let image = self.image.clone();
                tokio::task::spawn_blocking(move || image.write_all_at(&chunk_data, start))
                    .await
                    .map_err(std::io::Error::other)??;
                self.mark(i)?;
            }
        }
        let image = self.image.clone();
        tokio::task::spawn_blocking(move || image.write_all_at(&data, offset))
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }

    pub async fn flush(&self) -> Result<()> {
        let image = self.image.clone();
        tokio::task::spawn_blocking(move || image.sync_data())
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Download every missing chunk; then remove the marker file.
    pub async fn hydrate(self: &Arc<Self>, concurrency: usize) -> Result<()> {
        let started = std::time::Instant::now();
        let todo: Vec<usize> = {
            let present = self.present.lock().unwrap();
            (0..present.len()).filter(|i| !present[*i]).collect()
        };
        futures::stream::iter(todo)
            .map(|i| {
                let this = self.clone();
                async move { this.ensure(i, false).await }
            })
            .buffer_unordered(concurrency.max(1))
            .try_collect::<()>()
            .await?;
        self.flush().await?;
        let _ = std::fs::remove_file(&self.present_path);
        tracing::info!(
            ms = started.elapsed().as_millis() as u64,
            bytes = self.size,
            "lazy image complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;
    use crate::tests::noise;
    use crate::SnapshotMeta;

    async fn setup() -> (Remote, Manifest, tempfile::TempDir, Vec<u8>) {
        let remote = Remote::from_store(Arc::new(InMemory::new()), "");
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.ext4");
        let f = std::fs::File::create(&src).unwrap();
        f.set_len(24 << 20).unwrap();
        f.write_all_at(&noise(7, 9 << 20), 3 << 20).unwrap();
        let expected = std::fs::read(&src).unwrap();
        let meta = SnapshotMeta {
            id: "vsnap-lazy".into(),
            ..Default::default()
        };
        let (manifest, _) = remote.push_block(&src, &meta).await.unwrap();
        (remote, manifest, dir, expected)
    }

    #[tokio::test]
    async fn reads_fetch_only_what_they_touch() {
        let (remote, manifest, dir, expected) = setup().await;
        let img = LazyImage::open(remote, &manifest, &dir.path().join("lazy.ext4")).unwrap();
        let stored = manifest.chunks.len();
        assert_eq!(img.missing(), stored);
        // A read inside zero chunks downloads nothing.
        assert_eq!(img.read(20 << 20, 4096).await.unwrap(), vec![0; 4096]);
        assert_eq!(img.missing(), stored);
        // A read across two stored chunks downloads exactly those.
        let got = img.read((4 << 20) - 100, 200).await.unwrap();
        assert_eq!(got, expected[(4 << 20) - 100..(4 << 20) + 100]);
        assert_eq!(img.missing(), stored - 2);
    }

    #[tokio::test]
    async fn writes_merge_with_remote_data_and_survive_hydration() {
        let (remote, manifest, dir, mut expected) = setup().await;
        let path = dir.path().join("lazy.ext4");
        let img = LazyImage::open(remote.clone(), &manifest, &path).unwrap();
        // Partial write into a stored chunk: read-modify-write.
        img.write(5 << 20, b"hello".to_vec()).await.unwrap();
        expected[5 << 20..(5 << 20) + 5].copy_from_slice(b"hello");
        // A whole-chunk write needs no download.
        let whole = vec![9u8; 4 << 20];
        img.write(8 << 20, whole.clone()).await.unwrap();
        expected[8 << 20..12 << 20].copy_from_slice(&whole);
        drop(img);

        // Resume after a "restart", then finish.
        let img = LazyImage::open(remote, &manifest, &path).unwrap();
        img.hydrate(4).await.unwrap();
        assert!(img.is_complete());
        assert!(!present_path(&path).exists());
        assert_eq!(std::fs::read(&path).unwrap(), expected);
    }

    #[tokio::test]
    async fn bounds_are_checked() {
        let (remote, manifest, dir, _) = setup().await;
        let img = LazyImage::open(remote, &manifest, &dir.path().join("x.ext4")).unwrap();
        assert!(img.read((24 << 20) - 1, 2).await.is_err());
        assert!(img.write(24 << 20, vec![1]).await.is_err());
    }
}
