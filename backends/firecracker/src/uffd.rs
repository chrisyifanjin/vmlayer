//! Populate a restored guest's memory up front, over userfaultfd.
//!
//! With the default `File` memory backend Firecracker maps the snapshot's
//! memory file and the guest takes a page fault on the first touch of every
//! page. Under nested virtualization each of those faults is expensive: the
//! first heavy process after a restore spends seconds faulting (measured in
//! `docs/benchmarks.md`).
//!
//! With the `Uffd` backend Firecracker instead registers guest memory with a
//! userfaultfd and hands it to us over a Unix socket. This handler then
//! copies the whole snapshot into place in large batches right after the
//! restore, maps all-zero ranges to the shared zero page instead of copying
//! them, and serves any fault the guest takes meanwhile first. When every
//! page is present it closes the userfaultfd and exits. This is the
//! approach of Firecracker's own "fault all" example handler, with zero-page
//! elision so untouched guest memory still costs nothing (decision 0013).

use std::fs::File;
use std::io::IoSliceMut;
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use serde::Deserialize;
use vmlayer_core::Result;

use crate::api::err;

/// Copy granularity for the background population.
const BATCH: u64 = 2 << 20;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
/// How long Firecracker has to connect after we start listening.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

#[repr(C)]
struct UffdioZeropage {
    range: UffdioRange,
    mode: u64,
    zeropage: i64,
}

// linux/userfaultfd.h
nix::ioctl_readwrite!(uffdio_copy, 0xAA, 0x03, UffdioCopy);
nix::ioctl_readwrite!(uffdio_zeropage, 0xAA, 0x04, UffdioZeropage);

/// One guest memory region as Firecracker describes it.
#[derive(Clone, Debug, Deserialize)]
struct Region {
    base_host_virt_addr: u64,
    size: u64,
    offset: u64,
    #[serde(default)]
    page_size: Option<u64>,
    /// Older Firecracker releases.
    #[serde(default)]
    page_size_kib: Option<u64>,
}

impl Region {
    fn page(&self) -> u64 {
        self.page_size
            .or(self.page_size_kib.map(|k| k * 1024))
            .filter(|&p| p >= 4096)
            .unwrap_or(4096)
    }

    fn contains(&self, addr: u64) -> bool {
        addr >= self.base_host_virt_addr && addr < self.base_host_virt_addr + self.size
    }
}

/// The handler thread for one restore. Dropping it stops the thread.
pub(crate) struct UffdServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for UffdServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // The thread checks the flag at least every 100 ms.
            let _ = t.join();
        }
    }
}

impl UffdServer {
    /// Listen on `socket` (inside the jail, owned by the jail user) and serve
    /// the memory of snapshot file `mem` to the Firecracker that connects.
    pub(crate) fn listen(socket: &Path, mem: &Path, uid: u32, gid: u32) -> Result<Self> {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket)
            .map_err(|e| err(format!("uffd socket {}: {e}", socket.display())))?;
        std::os::unix::fs::chown(socket, Some(uid), Some(gid))?;
        listener.set_nonblocking(true)?;
        let mem = File::open(mem)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let label = socket.display().to_string();
        let thread = std::thread::Builder::new()
            .name("vmlayer-uffd".into())
            .spawn(move || {
                if let Err(e) = serve(listener, mem, &flag) {
                    if !flag.load(Ordering::Relaxed) {
                        tracing::warn!(socket = %label, error = %e, "memory population stopped");
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

fn serve(listener: UnixListener, mem: File, stop: &AtomicBool) -> std::io::Result<()> {
    let started = Instant::now();
    let stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Relaxed) || started.elapsed() > CONNECT_TIMEOUT {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(e),
        }
    };
    let (regions, uffd) = receive(&stream)?;
    drop(listener);
    let len = mem.metadata()?.len();
    let len = NonZeroUsize::new(len as usize)
        .ok_or_else(|| std::io::Error::other("empty memory file"))?;
    // SAFETY: a fresh read-only private mapping of a file we keep open.
    let base = unsafe {
        mmap(
            None,
            len,
            ProtFlags::PROT_READ,
            MapFlags::MAP_PRIVATE,
            mem.as_fd(),
            0,
        )
    }
    .map_err(std::io::Error::from)?;
    let src = base.as_ptr() as u64;
    let result = populate(&uffd, &regions, src, len.get() as u64, stop, started);
    // SAFETY: unmapping exactly what we mapped; nothing else holds pointers into it.
    unsafe {
        let _ = munmap(base, len.get());
    }
    drop(stream);
    result
}

/// Firecracker sends the region layout as JSON with the userfaultfd attached.
fn receive(stream: &std::os::unix::net::UnixStream) -> std::io::Result<(Vec<Region>, OwnedFd)> {
    stream.set_nonblocking(false)?;
    let mut buf = vec![0u8; 16 * 1024];
    let mut cmsg = nix::cmsg_space!(RawFd);
    let (bytes, fd) = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg),
            MsgFlags::empty(),
        )
        .map_err(std::io::Error::from)?;
        let mut fd = None;
        for c in msg.cmsgs().map_err(std::io::Error::from)? {
            if let ControlMessageOwned::ScmRights(fds) = c {
                fd = fds.first().copied();
            }
        }
        (msg.bytes, fd)
    };
    let fd = fd.ok_or_else(|| std::io::Error::other("Firecracker sent no userfaultfd"))?;
    // SAFETY: the kernel just installed this descriptor for us.
    let uffd = unsafe { OwnedFd::from_raw_fd(fd) };
    let regions: Vec<Region> = serde_json::from_slice(&buf[..bytes])
        .map_err(|e| std::io::Error::other(format!("region layout: {e}")))?;
    Ok((regions, uffd))
}

fn populate(
    uffd: &OwnedFd,
    regions: &[Region],
    src_base: u64,
    src_len: u64,
    stop: &AtomicBool,
    started: Instant,
) -> std::io::Result<()> {
    let (mut copied, mut zeroed, mut faults) = (0u64, 0u64, 0u64);
    let mut batches = regions.iter().flat_map(|r| {
        (0..r.size)
            .step_by(BATCH as usize)
            .map(move |off| (r.clone(), off, BATCH.min(r.size - off)))
    });
    let mut done = false;
    while !stop.load(Ordering::Relaxed) {
        // Faults first: a guest thread is waiting on each one.
        let wait = if done { 100 } else { 0 };
        while let Some(addr) = next_fault(uffd, wait)? {
            faults += 1;
            if let Some(r) = regions.iter().find(|r| r.contains(addr)) {
                let page = addr & !(r.page() - 1);
                let off = page - r.base_host_virt_addr;
                copy(uffd, r, off, r.page(), src_base, src_len)?;
            }
            if done {
                break;
            }
        }
        if done {
            // Every page is present: no more faults can come.
            break;
        }
        match batches.next() {
            Some((r, off, len)) => {
                let src = src_base + r.offset + off;
                if r.offset + off + len > src_len {
                    return Err(std::io::Error::other(
                        "memory file shorter than guest memory",
                    ));
                }
                // SAFETY: within our read-only mapping (bounds checked above).
                let bytes = unsafe { std::slice::from_raw_parts(src as *const u8, len as usize) };
                if is_zero(bytes) {
                    zeropage(uffd, r.base_host_virt_addr + off, len)?;
                    zeroed += len;
                } else {
                    copy(uffd, &r, off, len, src_base, src_len)?;
                    copied += len;
                }
            }
            None => done = true,
        }
    }
    tracing::info!(
        ms = started.elapsed().as_millis() as u64,
        copied_mib = copied >> 20,
        zero_mib = zeroed >> 20,
        faults,
        "guest memory populated"
    );
    Ok(())
}

fn next_fault(uffd: &OwnedFd, wait_ms: u16) -> std::io::Result<Option<u64>> {
    let mut fds = [PollFd::new(uffd.as_fd(), PollFlags::POLLIN)];
    if poll(&mut fds, PollTimeout::from(wait_ms)).map_err(std::io::Error::from)? == 0 {
        return Ok(None);
    }
    let mut msg = [0u8; 32];
    // SAFETY: reading into a correctly sized local buffer.
    let n = unsafe { libc::read(uffd.as_raw_fd(), msg.as_mut_ptr().cast(), msg.len()) };
    if n < 0 {
        let e = Errno::last();
        return if e == Errno::EAGAIN {
            Ok(None)
        } else {
            Err(e.into())
        };
    }
    if n as usize != msg.len() || msg[0] != UFFD_EVENT_PAGEFAULT {
        return Ok(None);
    }
    Ok(Some(u64::from_ne_bytes(
        msg[16..24].try_into().expect("8 bytes"),
    )))
}

/// Copy `len` bytes at region offset `off`, skipping pages already present.
fn copy(
    uffd: &OwnedFd,
    r: &Region,
    off: u64,
    len: u64,
    src_base: u64,
    src_len: u64,
) -> std::io::Result<()> {
    let (dst, src) = (r.base_host_virt_addr + off, src_base + r.offset + off);
    if r.offset + off + len > src_len {
        return Err(std::io::Error::other("fault outside the memory file"));
    }
    let mut done = 0;
    while done < len {
        let mut arg = UffdioCopy {
            dst: dst + done,
            src: src + done,
            len: len - done,
            mode: 0,
            copy: 0,
        };
        // SAFETY: ioctl on our userfaultfd with a valid argument struct.
        match unsafe { uffdio_copy(uffd.as_raw_fd(), &mut arg) } {
            Ok(_) => done += arg.copy.max(0) as u64,
            Err(e) => {
                if arg.copy > 0 {
                    done += arg.copy as u64;
                    continue;
                }
                match e {
                    // A fault already brought this page in.
                    Errno::EEXIST => done += r.page(),
                    Errno::EAGAIN => continue,
                    other => return Err(other.into()),
                }
            }
        }
    }
    Ok(())
}

fn zeropage(uffd: &OwnedFd, start: u64, len: u64) -> std::io::Result<()> {
    let mut arg = UffdioZeropage {
        range: UffdioRange { start, len },
        mode: 0,
        zeropage: 0,
    };
    // SAFETY: ioctl on our userfaultfd with a valid argument struct.
    match unsafe { uffdio_zeropage(uffd.as_raw_fd(), &mut arg) } {
        Ok(_) => Ok(()),
        // Some pages in the range were faulted in already: fall back to
        // page-sized zero mappings around them.
        Err(Errno::EEXIST) | Err(Errno::EAGAIN) => {
            let mut at = start;
            while at < start + len {
                let mut one = UffdioZeropage {
                    range: UffdioRange {
                        start: at,
                        len: 4096,
                    },
                    mode: 0,
                    zeropage: 0,
                };
                // SAFETY: as above.
                match unsafe { uffdio_zeropage(uffd.as_raw_fd(), &mut one) } {
                    Ok(_) | Err(Errno::EEXIST) => at += 4096,
                    Err(Errno::EAGAIN) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn is_zero(data: &[u8]) -> bool {
    // SAFETY: reinterpreting initialized bytes as u64 words is sound.
    let (head, words, tail) = unsafe { data.align_to::<u64>() };
    head.iter().all(|&b| b == 0) && words.iter().all(|&w| w == 0) && tail.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_layout_parses_both_formats() {
        let new: Vec<Region> = serde_json::from_str(
            r#"[{"base_host_virt_addr":4096,"size":8192,"offset":0,"page_size":4096}]"#,
        )
        .unwrap();
        assert_eq!(new[0].page(), 4096);
        assert!(new[0].contains(4096) && new[0].contains(12287) && !new[0].contains(12288));
        let old: Vec<Region> = serde_json::from_str(
            r#"[{"base_host_virt_addr":0,"size":4194304,"offset":0,"page_size_kib":2048}]"#,
        )
        .unwrap();
        assert_eq!(old[0].page(), 2 << 20);
    }
}
