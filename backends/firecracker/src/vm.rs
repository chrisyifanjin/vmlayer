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

/// The base image with the guest agent injected, built once per (image,
/// agent binary) and cached under `<data>/prepared/`. Sandboxes clone this,
/// so the injection cost is not paid on every boot.
pub async fn prepared_image(config: &Config, base: &Path) -> Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let Some(agent) = &config.guest_agent else {
        return Ok(base.to_path_buf());
    };
    let stamp = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (base, stamp(base), agent, stamp(agent)).hash(&mut h);
    let dir = config.data_dir.join("prepared");
    let prepared = dir.join(format!("{:016x}.ext4", h.finish()));
    if prepared.exists() {
        // Mark as recently used, for pruning.
        if let Ok(f) = std::fs::File::options().write(true).open(&prepared) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        return Ok(prepared);
    }
    tokio::fs::create_dir_all(&dir).await?;
    // Each concurrent builder needs its own image. Publish only after
    // injection is complete, without replacing another builder's result.
    let tmp = dir.join(format!(".{}.partial", uuid::Uuid::now_v7()));
    let built: Result<()> = async {
        clone_file(base, &tmp).await?;
        inject_agent(&tmp, agent).await?;
        match tokio::fs::hard_link(&tmp, &prepared).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&tmp).await;
    if built.is_ok() {
        prune_prepared(&dir, PREPARED_KEEP).await;
    }
    built.map(|_| prepared)
}

/// Prepared images kept, most recently used first. A new guest agent or
/// image version makes a new one; without pruning they pile up forever.
/// Running VMs boot from clones, so deleting a prepared image is safe.
const PREPARED_KEEP: usize = 8;

async fn prune_prepared(dir: &Path, keep: usize) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let mut images = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "ext4")
            && !e.file_name().to_string_lossy().starts_with('.')
        {
            let used = e
                .metadata()
                .await
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            images.push((used, path));
        }
    }
    images.sort_by_key(|i| std::cmp::Reverse(i.0));
    for (_, path) in images.into_iter().skip(keep) {
        let _ = tokio::fs::remove_file(&path).await;
        tracing::info!(image = %path.display(), "pruned prepared image");
    }
}

/// Default kernel command line. `pci=off` keeps the device layout stable for
/// snapshots (hot attach would need PCI; see Capabilities::hot_attach).
pub fn boot_args(config: &Config, hostname: &str) -> String {
    // `quiet`: every character the kernel writes to the serial console is a
    // VM exit, and boot messages are most of the boot time (far more so under
    // nested virtualization). Warnings and panics still reach console.log;
    // put `loglevel=7` in `kernel_args` to see everything.
    let mut args = String::from(
        "console=ttyS0 quiet reboot=k panic=1 pci=off nomodules random.trust_cpu=on i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd",
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
    /// The VM's shim (decision 0019), when this daemon started it.
    child: Option<tokio::process::Child>,
    /// Firecracker's pid.
    pub pid: u32,
    pub shim: crate::shim::Client,
}

/// What the shim runs for a VM besides Firecracker itself.
#[derive(Default)]
pub struct Services {
    pub uffd: Option<crate::shim::UffdSpec>,
    pub dns: Option<crate::shim::DnsSpec>,
    pub lazy: Vec<crate::shim::LazySpec>,
    pub deadline_ms: Option<u64>,
}

/// Files of a VM's shim, next to (not inside) its chroot.
pub fn shim_paths(jail_root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = jail_root.parent().unwrap_or(jail_root);
    (
        dir.join("shim.json"),
        dir.join("shim.sock"),
        dir.join("shim.exit"),
        dir.join("shim.log"),
    )
}

impl Vm {
    /// Start the VM's shim, which starts the jailer (and so Firecracker,
    /// inside the VM's network namespace) and the VM's services. The chroot
    /// must already be populated.
    pub async fn launch(
        config: &Config,
        id: &str,
        lease: &net::Lease,
        services: Services,
    ) -> Result<Self> {
        let jail_root = config.jail_root(&vmlayer_core::SandboxId(id.to_string()));
        let chroot_base = config.data_dir.join("jail");
        tokio::fs::create_dir_all(&jail_root).await?;
        let (spec_path, control, exit, log) = shim_paths(&jail_root);
        let jailer = vec![
            config.jailer_bin.display().to_string(),
            "--id".into(),
            id.into(),
            "--exec-file".into(),
            config.firecracker_bin.display().to_string(),
            "--uid".into(),
            config.jail_uid.to_string(),
            "--gid".into(),
            config.jail_gid.to_string(),
            "--chroot-base-dir".into(),
            chroot_base.display().to_string(),
            "--cgroup-version".into(),
            "2".into(),
            "--netns".into(),
            format!("/var/run/netns/{}", lease.netns),
            "--".into(),
            "--api-sock".into(),
            format!("/{API_SOCK}"),
        ];
        let spec = crate::shim::ShimSpec {
            id: id.to_string(),
            jailer,
            console: console_log_path(&jail_root),
            pid_file: jail_root.join(PID_FILE),
            control: control.clone(),
            exit_file: exit,
            jail_uid: config.jail_uid,
            jail_gid: config.jail_gid,
            expose_target: lease.ns_ip,
            uffd: services.uffd,
            dns: services.dns,
            lazy: services.lazy,
            deadline_ms: services.deadline_ms,
        };
        let json = serde_json::to_vec_pretty(&spec).map_err(|e| api::err(e.to_string()))?;
        tokio::fs::write(&spec_path, json).await?;
        tokio::fs::write(jail_root.join(LEASE_FILE), lease.index.to_string()).await?;
        let shim_log = std::fs::File::create(&log)?;
        let mut cmd = Command::new(&config.shim_bin);
        cmd.arg(&spec_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(shim_log.try_clone()?))
            .stderr(std::process::Stdio::from(shim_log))
            .kill_on_drop(false);
        // Its own session: a daemon stop or crash must not take the VM along.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| api::err(format!("spawning {}: {e}", config.shim_bin.display())))?;
        let shim = crate::shim::Client { socket: control };
        let status = match shim.wait_ready(Duration::from_secs(20), &log).await {
            Ok(s) => s,
            Err(e) => {
                let mut child = child;
                let _ = child.kill().await;
                return Err(e);
            }
        };
        let pid = status.pid.unwrap_or(0);
        let api = Client::new(jail_root.join(API_SOCK));
        let mut vm = Self {
            jail_root: jail_root.clone(),
            api,
            child: Some(child),
            pid,
            shim,
        };
        if let Err(e) = vm.api.wait_ready(Duration::from_secs(10)).await {
            let tail = console_tail(&jail_root).await;
            let _ = vm.kill().await;
            return Err(api::err(format!("{e}; jailer/firecracker output: {tail}")));
        }
        Ok(vm)
    }

    /// Reattach to a VM (and its shim) started by a previous daemon.
    pub async fn attach(config: &Config, id: &str) -> Result<Self> {
        let jail_root = config.jail_root(&vmlayer_core::SandboxId(id.to_string()));
        let (_, control, _, _) = shim_paths(&jail_root);
        let shim = crate::shim::Client { socket: control };
        let status = shim.call(&crate::shim::Request::Status).await?;
        let api = Client::new(jail_root.join(API_SOCK));
        api.info().await?;
        Ok(Self {
            jail_root,
            api,
            child: None,
            pid: status.pid.unwrap_or(0),
            shim,
        })
    }

    pub fn lease_index(jail_root: &Path) -> Option<u32> {
        std::fs::read_to_string(jail_root.join(LEASE_FILE))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Kill the VM through its shim and wait for the shim to finish.
    pub async fn kill(&mut self) -> Result<()> {
        let (_, control, exit, _) = shim_paths(&self.jail_root);
        let asked = self.shim.call(&crate::shim::Request::Stop).await.is_ok();
        if !asked && self.pid > 1 {
            // No shim (it crashed): kill Firecracker directly.
            unsafe {
                libc_kill(self.pid as i32, 9);
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(15), child.wait()).await;
        } else {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while control.exists() && !exit.exists() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
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

    #[tokio::test]
    async fn prepared_images_are_pruned_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5u64 {
            let p = dir.path().join(format!("{i:016x}.ext4"));
            std::fs::write(&p, b"x").unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + i);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        prune_prepared(dir.path(), 2).await;
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["0000000000000003.ext4", "0000000000000004.ext4"]);
    }

    #[test]
    fn boot_args_contain_the_essentials() {
        let cfg = crate::test_config();
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
