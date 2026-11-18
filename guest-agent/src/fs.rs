//! Mounts and filesystem freezing inside the guest.

use std::os::fd::AsRawFd;
use std::path::Path;

use nix::mount::MsFlags;
use vmlayer_core::{Error, Result};

const FIFREEZE: libc::c_ulong = 0xC004_5877;
const FITHAW: libc::c_ulong = 0xC004_5878;

pub fn mount(source: &str, path: &str, fstype: &str, read_only: bool) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let mut flags = MsFlags::empty();
    if read_only {
        flags |= MsFlags::MS_RDONLY;
    }
    let fstype = if fstype.is_empty() {
        detect_fstype(source)
    } else {
        fstype.to_string()
    };
    nix::mount::mount(
        Some(source),
        path,
        Some(fstype.as_str()),
        flags,
        None::<&str>,
    )
    .map_err(|e| Error::Guest(format!("mount {source} at {path} ({fstype}): {e}")))
}

pub fn unmount(path: &str) -> Result<()> {
    nix::mount::umount(path).map_err(|e| Error::Guest(format!("umount {path}: {e}")))
}

/// Block devices from vmlayer are ext4 unless told otherwise; virtiofs tags
/// contain no slash.
fn detect_fstype(source: &str) -> String {
    if source.starts_with("/dev/") {
        "ext4".into()
    } else {
        "virtiofs".into()
    }
}

fn freeze_ioctl(path: &Path, req: libc::c_ulong, what: &str) -> Result<()> {
    let dir = std::fs::File::open(path)?;
    let arg: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(dir.as_raw_fd(), req as _, &arg) };
    if rc == 0 {
        Ok(())
    } else {
        let e = std::io::Error::last_os_error();
        // EBUSY on freeze = already frozen; EINVAL on thaw = not frozen.
        match e.raw_os_error() {
            Some(libc::EBUSY) | Some(libc::EINVAL) => Ok(()),
            _ => Err(Error::Guest(format!("{what} {}: {e}", path.display()))),
        }
    }
}

pub fn freeze(path: &Path) -> Result<()> {
    freeze_ioctl(path, FIFREEZE, "freeze")
}

pub fn thaw(path: &Path) -> Result<()> {
    freeze_ioctl(path, FITHAW, "thaw")
}

pub fn rescan_pci() -> Result<()> {
    std::fs::write("/sys/bus/pci/rescan", "1").map_err(|e| Error::Guest(format!("pci rescan: {e}")))
}

/// `/etc/hosts` with `hosts` (name -> address) set: earlier lines naming
/// any of them lose that name, then one line per name is appended.
pub fn with_hosts(current: &str, hosts: &std::collections::HashMap<String, String>) -> String {
    let mut out = String::new();
    for line in current.lines() {
        let (entry, comment) = line.split_once('#').unwrap_or((line, ""));
        let mut fields = entry.split_whitespace();
        let Some(addr) = fields.next() else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let names: Vec<&str> = fields.filter(|n| !hosts.contains_key(*n)).collect();
        if names.is_empty() {
            continue;
        }
        out.push_str(&format!("{addr} {}", names.join(" ")));
        if !comment.is_empty() {
            out.push_str(&format!(" #{comment}"));
        }
        out.push('\n');
    }
    let mut names: Vec<_> = hosts.iter().collect();
    names.sort();
    for (name, addr) in names {
        out.push_str(&format!("{addr} {name}\n"));
    }
    out
}

#[cfg(test)]
mod hosts_tests {
    use super::*;

    #[test]
    fn hosts_entries_are_replaced() {
        let hosts =
            std::collections::HashMap::from([("vmlayer.host".to_string(), "10.0.0.1".to_string())]);
        let before = "127.0.0.1 localhost\n10.9.9.9 vmlayer.host other # old\n\n# c\n";
        let after = with_hosts(before, &hosts);
        assert_eq!(
            after,
            "127.0.0.1 localhost\n10.9.9.9 other # old\n\n# c\n10.0.0.1 vmlayer.host\n"
        );
        assert_eq!(with_hosts(&after, &hosts), after, "idempotent");
    }
}
