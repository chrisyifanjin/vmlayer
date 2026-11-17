//! Serve a [`LazyImage`] to the kernel as `/dev/nbdN` (decision 0016).
//!
//! The device is configured with the NBD ioctls over one end of a Unix
//! socketpair, so no `nbd-client` tool and no TCP are involved; the daemon
//! serves the other end. With ioctl configuration there is no handshake: the
//! socket carries the transmission phase from the first byte.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use vmlayer_blob::LazyImage;
use vmlayer_core::{Error, Result};

const REQUEST_MAGIC: u32 = 0x2560_9513;
const REPLY_MAGIC: u32 = 0x6744_6698;
const CMD_READ: u16 = 0;
const CMD_WRITE: u16 = 1;
const CMD_DISC: u16 = 2;
const CMD_FLUSH: u16 = 3;
const CMD_TRIM: u16 = 4;
const EIO: u32 = 5;
const EINVAL: u32 = 22;
/// Largest request we accept (the kernel's maximum is far below this).
const MAX_REQUEST: u32 = 32 << 20;

// linux/nbd.h: _IO(0xab, n)
const NBD_SET_SOCK: u64 = 0xab00;
const NBD_SET_BLKSIZE: u64 = 0xab01;
const NBD_DO_IT: u64 = 0xab03;
const NBD_CLEAR_SOCK: u64 = 0xab04;
const NBD_CLEAR_QUE: u64 = 0xab05;
const NBD_SET_SIZE_BLOCKS: u64 = 0xab07;
const NBD_DISCONNECT: u64 = 0xab08;
const NBD_SET_FLAGS: u64 = 0xab0a;
const FLAG_HAS_FLAGS: u64 = 1;
const FLAG_SEND_FLUSH: u64 = 1 << 2;
const FLAG_SEND_TRIM: u64 = 1 << 5;
const BLOCK: u64 = 4096;

/// Whether this host can serve lazy volumes (the `nbd` module is loaded).
pub fn available() -> bool {
    std::path::Path::new("/dev/nbd0").exists()
}

fn ioctl(fd: &File, request: u64, arg: u64) -> std::io::Result<()> {
    // SAFETY: NBD ioctls on a device we opened, with integer arguments.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg as libc::c_ulong) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A connected `/dev/nbdN`. Dropping it disconnects the device.
pub struct NbdExport {
    pub device: PathBuf,
    dev: Arc<File>,
    kernel: Option<std::thread::JoinHandle<()>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for NbdExport {
    fn drop(&mut self) {
        let _ = ioctl(&self.dev, NBD_DISCONNECT, 0);
        if let Some(t) = self.kernel.take() {
            let _ = t.join();
        }
        self.server.abort();
    }
}

impl NbdExport {
    /// Connect the first free NBD device to `image`.
    pub fn attach(image: Arc<LazyImage>) -> Result<Self> {
        if image.size() % BLOCK != 0 {
            return Err(Error::Guest(
                "lazy images must be a multiple of 4 KiB".into(),
            ));
        }
        for n in 0..1024 {
            let device = PathBuf::from(format!("/dev/nbd{n}"));
            if !device.exists() {
                break;
            }
            if std::path::Path::new(&format!("/sys/block/nbd{n}/pid")).exists() {
                continue; // in use
            }
            let Ok(dev) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&device)
            else {
                continue;
            };
            let (kernel_end, server_end) = std::os::unix::net::UnixStream::pair()?;
            if ioctl(&dev, NBD_SET_SOCK, kernel_end.as_raw_fd() as u64).is_err() {
                continue; // taken meanwhile
            }
            let configure = || -> std::io::Result<()> {
                ioctl(&dev, NBD_SET_BLKSIZE, BLOCK)?;
                ioctl(&dev, NBD_SET_SIZE_BLOCKS, image.size() / BLOCK)?;
                ioctl(
                    &dev,
                    NBD_SET_FLAGS,
                    FLAG_HAS_FLAGS | FLAG_SEND_FLUSH | FLAG_SEND_TRIM,
                )
            };
            if let Err(e) = configure() {
                let _ = ioctl(&dev, NBD_CLEAR_SOCK, 0);
                return Err(Error::Guest(format!(
                    "configuring {}: {e}",
                    device.display()
                )));
            }
            server_end.set_nonblocking(true)?;
            let stream = tokio::net::UnixStream::from_std(server_end)?;
            let server = tokio::spawn(serve(stream, image.clone()));
            let dev = Arc::new(dev);
            let kdev = dev.clone();
            let kernel = std::thread::Builder::new()
                .name("vmlayer-nbd".into())
                .spawn(move || {
                    // Blocks until the device is disconnected.
                    let _ = ioctl(&kdev, NBD_DO_IT, 0);
                    let _ = ioctl(&kdev, NBD_CLEAR_QUE, 0);
                    let _ = ioctl(&kdev, NBD_CLEAR_SOCK, 0);
                    drop(kernel_end);
                })?;
            // The device is usable once the kernel thread runs; wait for it.
            for _ in 0..200 {
                if std::path::Path::new(&format!("/sys/block/nbd{n}/pid")).exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            tracing::info!(device = %device.display(), bytes = image.size(), "lazy volume exported");
            return Ok(Self {
                device,
                dev,
                kernel: Some(kernel),
                server,
            });
        }
        Err(Error::ResourceExhausted("no free /dev/nbd device".into()))
    }
}

async fn serve(stream: tokio::net::UnixStream, image: Arc<LazyImage>) {
    let (mut rd, wr) = stream.into_split();
    let wr = Arc::new(Mutex::new(wr));
    loop {
        let mut header = [0u8; 28];
        if rd.read_exact(&mut header).await.is_err() {
            return;
        }
        let magic = u32::from_be_bytes(header[0..4].try_into().unwrap());
        let kind = u16::from_be_bytes(header[6..8].try_into().unwrap());
        let handle: [u8; 8] = header[8..16].try_into().unwrap();
        let offset = u64::from_be_bytes(header[16..24].try_into().unwrap());
        let len = u32::from_be_bytes(header[24..28].try_into().unwrap());
        if magic != REQUEST_MAGIC || len > MAX_REQUEST {
            tracing::warn!(magic, len, "bad NBD request; closing");
            return;
        }
        let payload = if kind == CMD_WRITE {
            let mut data = vec![0u8; len as usize];
            if rd.read_exact(&mut data).await.is_err() {
                return;
            }
            Some(data)
        } else {
            None
        };
        if kind == CMD_DISC {
            return;
        }
        let (image, wr) = (image.clone(), wr.clone());
        tokio::spawn(async move {
            let (error, data) = match kind {
                CMD_READ => match image.read(offset, len as usize).await {
                    Ok(d) => (0, Some(d)),
                    Err(e) => {
                        tracing::warn!(offset, len, error = %e, "lazy read failed");
                        (EIO, None)
                    }
                },
                CMD_WRITE => match image.write(offset, payload.unwrap_or_default()).await {
                    Ok(()) => (0, None),
                    Err(e) => {
                        tracing::warn!(offset, len, error = %e, "lazy write failed");
                        (EIO, None)
                    }
                },
                CMD_FLUSH => (if image.flush().await.is_ok() { 0 } else { EIO }, None),
                CMD_TRIM => (0, None),
                _ => (EINVAL, None),
            };
            let mut reply = Vec::with_capacity(16 + data.as_ref().map_or(0, Vec::len));
            reply.extend_from_slice(&REPLY_MAGIC.to_be_bytes());
            reply.extend_from_slice(&error.to_be_bytes());
            reply.extend_from_slice(&handle);
            if let Some(d) = data {
                reply.extend_from_slice(&d);
            }
            let _ = wr.lock().await.write_all(&reply).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    use object_store::memory::InMemory;
    use vmlayer_blob::{Remote, SnapshotMeta};

    use super::*;

    fn request(kind: u16, handle: u64, offset: u64, len: u32) -> Vec<u8> {
        let mut r = REQUEST_MAGIC.to_be_bytes().to_vec();
        r.extend_from_slice(&0u16.to_be_bytes());
        r.extend_from_slice(&kind.to_be_bytes());
        r.extend_from_slice(&handle.to_be_bytes());
        r.extend_from_slice(&offset.to_be_bytes());
        r.extend_from_slice(&len.to_be_bytes());
        r
    }

    #[tokio::test]
    async fn speaks_the_transmission_protocol() {
        let remote = Remote::from_store(Arc::new(InMemory::new()), "");
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let f = std::fs::File::create(&src).unwrap();
        f.set_len(8 << 20).unwrap();
        f.write_all_at(b"remote bytes", 1 << 20).unwrap();
        let (m, _) = remote
            .push_block(
                &src,
                &SnapshotMeta {
                    id: "v".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let image = LazyImage::open(remote, &m, &dir.path().join("img")).unwrap();

        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(serve(server, image));
        let (mut rd, mut wr) = client.into_split();

        let mut write = request(CMD_WRITE, 7, 2 << 20, 5);
        write.extend_from_slice(b"local");
        wr.write_all(&write).await.unwrap();
        let mut reply = [0u8; 16];
        rd.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply[4..8], &[0, 0, 0, 0]);
        assert_eq!(u64::from_be_bytes(reply[8..16].try_into().unwrap()), 7);

        for (handle, offset, want) in [
            (8u64, 1u64 << 20, &b"remote bytes"[..5]),
            (9, 2 << 20, b"local"),
        ] {
            wr.write_all(&request(CMD_READ, handle, offset, 5))
                .await
                .unwrap();
            let mut reply = [0u8; 21];
            rd.read_exact(&mut reply).await.unwrap();
            assert_eq!(
                u32::from_be_bytes(reply[0..4].try_into().unwrap()),
                REPLY_MAGIC
            );
            assert_eq!(u64::from_be_bytes(reply[8..16].try_into().unwrap()), handle);
            assert_eq!(&reply[16..], want);
        }
        // Reading past the end is an I/O error, not a crash.
        wr.write_all(&request(CMD_READ, 10, 8 << 20, 4096))
            .await
            .unwrap();
        let mut reply = [0u8; 16];
        rd.read_exact(&mut reply).await.unwrap();
        assert_eq!(u32::from_be_bytes(reply[4..8].try_into().unwrap()), EIO);
    }
}
