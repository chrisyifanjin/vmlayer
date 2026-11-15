//! The jail and the Firecracker process.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use vmlayer_core::Result;

use crate::api::{self, Client};
use crate::{net, Config};

/// Fixed names inside the chroot. Snapshots restore into a fresh jail with
/// the same names, so no drive paths need patching.
pub const KERNEL: &str = "vmlinux";
pub const ROOT_DISK: &str = "root.ext4";
pub const VSOCK: &str = "v.sock";
pub const API_SOCK: &str = "api.sock";
pub const PID_FILE: &str = "vm.pid";
pub const LEASE_FILE: &str = "lease";
pub const AGENT_IN_GUEST: &str = "/vmlayer-guest-agent";
/// Serial console and VMM log, kept beside the chroot (outside the jail).
pub const CONSOLE_LOG: &str = "console.log";

pub fn console_log_path(jail_root: &Path) -> PathBuf {
    jail_root.parent().unwrap_or(jail_root).join(CONSOLE_LOG)
}

/// Last part of the console log, for error messages when a boot fails.
pub async fn console_tail(jail_root: &Path) -> String {
    match tokio::fs::read(console_log_path(jail_root)).await {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(2000);
            String::from_utf8_lossy(&bytes[start..]).trim().to_string()
        }
        Err(_) => String::new(),
    }
}

/// Give a file or directory to the uid/gid Firecracker runs as in the jail.
pub fn chown_for_jail(path: &Path, uid: u32, gid: u32) -> Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
        .map_err(|e| api::err(format!("chown {}: {e}", path.display())))
}

pub fn vol_disk(i: usize) -> String {
    format!("vol{i}.ext4")
}

/// Copy-on-write copy of a file (reflink where the FS allows, else a copy).
pub async fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    let out = Command::new("cp")
        .args(["--reflink=auto", "--sparse=always"])
        .arg(src)
        .arg(dst)
        .output()
        .await?;
    if !out.status.success() {
        return Err(api::err(format!(
            "cloning {} to {}: {}",
            src.display(),
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Hard link, falling back to a reflink copy when across filesystems. Used
/// for files the VM must share with the host (kernel, snapshot memory).
pub async fn link_or_clone(src: &Path, dst: &Path) -> Result<()> {
    if tokio::fs::hard_link(src, dst).await.is_ok() {
        return Ok(());
    }
    clone_file(src, dst).await
}

/// Write the guest agent into an ext4 image without mounting it (debugfs
/// can write files and set inode fields unprivileged).
pub async fn inject_agent(image: &Path, agent: &Path) -> Result<()> {
    let script = format!(
        "rm {dst}\nwrite {src} {dst}\nset_inode_field {dst} mode 0100755\n",
        src = agent.display(),
        dst = AGENT_IN_GUEST
    );
    let out = Command::new("debugfs")
        .arg("-w")
        .arg(image)
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| api::err(format!("debugfs: {e} (install e2fsprogs)")))?;
    let mut child = out;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(script.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        return Err(api::err(format!(
            "debugfs failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Default kernel command line. `pci=off` keeps the device layout stable for
/// snapshots (hot attach would need PCI; see Capabilities::hot_attach).
pub fn boot_args(config: &Config, hostname: &str) -> String {
    let mut args = String::from(
        "console=ttyS0 reboot=k panic=1 pci=off nomodules random.trust_cpu=on i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd",
    );
    args.push_str(&format!(" init={AGENT_IN_GUEST}"));
    args.push_str(&format!(" {}", net::Lease::kernel_args(&config.dns)));
    args.push_str(&format!(
        " vmlayer.listen=vsock:{} vmlayer.hostname={hostname}",
        config.agent_port
    ));
    if !config.kernel_args.is_empty() {
        args.push(' ');
        args.push_str(&config.kernel_args);
    }
    args
}

pub struct Vm {
    pub jail_root: PathBuf,
    pub api: Client,
    child: Option<tokio::process::Child>,
    pub pid: u32,
}

impl Vm {
    /// Launch the jailer for `id` with the chroot already populated. The
    /// jailer execs into Firecracker inside the VM's network namespace.
    pub async fn launch(config: &Config, id: &str, lease: &net::Lease) -> Result<Self> {
        let jail_root = config.jail_root(&vmlayer_core::SandboxId(id.to_string()));
        let chroot_base = config.data_dir.join("jail");
        tokio::fs::create_dir_all(&jail_root).await?;
        // The guest's serial console and the VMM's own messages go here.
        let console = std::fs::File::create(console_log_path(&jail_root))?;
        let mut cmd = Command::new(&config.jailer_bin);
        cmd.arg("--id")
            .arg(id)
            .arg("--exec-file")
            .arg(&config.firecracker_bin)
            .arg("--uid")
            .arg(config.jail_uid.to_string())
            .arg("--gid")
            .arg(config.jail_gid.to_string())
            .arg("--chroot-base-dir")
            .arg(&chroot_base)
            .arg("--cgroup-version")
            .arg("2")
            .arg("--netns")
            .arg(format!("/var/run/netns/{}", lease.netns))
            .arg("--")
            .arg("--api-sock")
            .arg(format!("/{API_SOCK}"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(console.try_clone()?))
            .stderr(std::process::Stdio::from(console))
            .kill_on_drop(false);
        let child = cmd
            .spawn()
            .map_err(|e| api::err(format!("spawning jailer: {e}")))?;
        let pid = child.id().unwrap_or(0);
        tokio::fs::write(jail_root.join(PID_FILE), pid.to_string()).await?;
        tokio::fs::write(jail_root.join(LEASE_FILE), lease.index.to_string()).await?;
        let api = Client::new(jail_root.join(API_SOCK));
        if let Err(e) = api.wait_ready(Duration::from_secs(10)).await {
            let tail = console_tail(&jail_root).await;
            let mut child = child;
            let _ = child.kill().await;
            return Err(api::err(format!("{e}; jailer/firecracker output: {tail}")));
        }
        Ok(Self {
            jail_root,
            api,
            child: Some(child),
            pid,
        })
    }

    /// Reattach to a VM launched by a previous daemon.
    pub async fn attach(config: &Config, id: &str) -> Result<Self> {
        let jail_root = config.jail_root(&vmlayer_core::SandboxId(id.to_string()));
        let pid: u32 = tokio::fs::read_to_string(jail_root.join(PID_FILE))
            .await
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let api = Client::new(jail_root.join(API_SOCK));
        api.info().await?;
        Ok(Self {
            jail_root,
            api,
            child: None,
            pid,
        })
    }

    pub fn lease_index(jail_root: &Path) -> Option<u32> {
        std::fs::read_to_string(jail_root.join(LEASE_FILE))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// SIGKILL the VMM and reap it.
    pub async fn kill(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        } else if self.pid > 1 {
            unsafe {
                libc_kill(self.pid as i32, 9);
            }
        }
        Ok(())
    }
}

unsafe fn libc_kill(pid: i32, sig: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_args_contain_the_essentials() {
        let cfg = Config {
            firecracker_bin: "/usr/bin/firecracker".into(),
            jailer_bin: "/usr/bin/jailer".into(),
            kernel: "/k".into(),
            kernel_args: "quiet".into(),
            data_dir: "/d".into(),
            jail_uid: 1000,
            jail_gid: 1000,
            subnet: net::Subnet::default(),
            dns: vec!["1.1.1.1".into()],
            agent_port: 5200,
            guest_agent: None,
        };
        let a = boot_args(&cfg, "sb-1");
        for needle in [
            "init=/vmlayer-guest-agent",
            "ip=172.16.0.2::172.16.0.1",
            "vmlayer.listen=vsock:5200",
            "vmlayer.hostname=sb-1",
            "pci=off",
            " quiet",
        ] {
            assert!(a.contains(needle), "{needle} missing from {a}");
        }
    }
}
