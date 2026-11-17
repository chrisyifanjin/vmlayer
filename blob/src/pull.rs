use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};

use crate::{Error, Kind, Manifest, Remote, Result};

impl Remote {
    /// Write snapshot `manifest` to `dest`: a new sparse image file for a
    /// block snapshot, an existing empty directory for a directory snapshot.
    pub async fn pull(&self, manifest: &Manifest, dest: &Path) -> Result<()> {
        match manifest.kind {
            Kind::Block => self.pull_block(manifest, dest).await,
            Kind::Dir => self.pull_dir(manifest, dest).await,
        }
    }

    async fn pull_block(&self, manifest: &Manifest, image: &Path) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(image)?;
        // Chunks not in the manifest were zero: leave them as holes.
        file.set_len(manifest.logical_size)?;
        let file = Arc::new(file);
        let this = self.clone();
        let shared = file.clone();
        futures::stream::iter(manifest.chunks.clone())
            .map(move |chunk| {
                let file = shared.clone();
                let this = this.clone();
                async move {
                    let data = this.get_chunk(&chunk).await?;
                    let offset = chunk.offset;
                    tokio::task::spawn_blocking(move || file.write_all_at(&data, offset))
                        .await
                        .map_err(std::io::Error::other)??;
                    Ok::<_, Error>(())
                }
            })
            .buffer_unordered(self.concurrency)
            .try_collect::<()>()
            .await?;
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }

    async fn pull_dir(&self, manifest: &Manifest, dir: &Path) -> Result<()> {
        let (reader, writer) = std::io::pipe()?;
        let dest = dir.to_path_buf();
        let unpack = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut archive = tar::Archive::new(reader);
            archive.set_preserve_permissions(true);
            archive.set_preserve_mtime(true);
            archive.set_unpack_xattrs(false);
            archive.set_preserve_ownerships(is_root());
            archive.set_overwrite(true);
            archive.unpack(&dest)?;
            // Drain the zero padding so the writer never sees a broken pipe.
            std::io::copy(&mut archive.into_inner(), &mut std::io::sink())?;
            Ok(())
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(self.concurrency);
        let write = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut writer = writer;
            while let Some(buf) = rx.blocking_recv() {
                writer.write_all(&buf)?;
            }
            Ok(())
        });
        // In order: the tar stream is the chunks concatenated.
        let this = self.clone();
        let fed: Result<()> = futures::stream::iter(manifest.chunks.clone())
            .map(move |chunk| {
                let this = this.clone();
                async move { this.get_chunk(&chunk).await }
            })
            .buffered(self.concurrency)
            .try_for_each(|data| {
                let tx = tx.clone();
                async move {
                    tx.send(data)
                        .await
                        .map_err(|_| Error::Io(std::io::Error::other("tar reader stopped early")))
                }
            })
            .await;
        drop(tx);
        let written = write.await.map_err(std::io::Error::other)?;
        let unpacked = unpack.await.map_err(std::io::Error::other)?;
        // The unpacker's error explains a broken pipe on the writer side.
        unpacked?;
        fed?;
        written?;
        Ok(())
    }
}

fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() == 0 }
}
