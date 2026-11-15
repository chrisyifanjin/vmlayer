//! PID 1 duties: minimal boot setup and orphan reaping.

use std::ffi::CString;

use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};

fn mount(src: &str, target: &str, fstype: &str, flags: nix::mount::MsFlags, data: Option<&str>) {
    let _ = std::fs::create_dir_all(target);
    if let Err(e) = nix::mount::mount(Some(src), target, Some(fstype), flags, data) {
        if e != nix::errno::Errno::EBUSY {
            eprintln!("vmlayer-guest-agent: mount {target}: {e}");
        }
    }
}

/// Mount the pseudo filesystems, bring up loopback, apply what the kernel
/// command line asks for. Idempotent; errors are logged, not fatal.
pub fn early_boot() {
    use nix::mount::MsFlags;
    mount(
        "proc",
        "/proc",
        "proc",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        None,
    );
    mount(
        "sysfs",
        "/sys",
        "sysfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        None,
    );
    mount(
        "devtmpfs",
        "/dev",
        "devtmpfs",
        MsFlags::MS_NOSUID,
        Some("mode=0755"),
    );
    let _ = std::fs::create_dir_all("/dev/pts");
    mount(
        "devpts",
        "/dev/pts",
        "devpts",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("gid=5,mode=0620,ptmxmode=0666"),
    );
    mount(
        "tmpfs",
        "/run",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=0755"),
    );
    mount(
        "tmpfs",
        "/tmp",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        None,
    );
    let _ = std::fs::create_dir_all("/dev/shm");
    mount(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        None,
    );
    let _ = nix::unistd::sethostname(
        cmdline_value("vmlayer.hostname").unwrap_or_else(|| "sandbox".into()),
    );
    loopback_up();
    if let Some(dns) = cmdline_value("vmlayer.dns") {
        let body: String = dns
            .split(',')
            .map(|s| format!("nameserver {s}\n"))
            .collect();
        let _ = std::fs::write("/etc/resolv.conf", body);
    }
}

/// `--listen` from the kernel command line (`vmlayer.listen=vsock:5200`).
pub fn listen_from_cmdline() -> Option<String> {
    cmdline_value("vmlayer.listen")
}

fn cmdline_value(key: &str) -> Option<String> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    cmdline.split_whitespace().find_map(|kv| {
        kv.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
            .map(String::from)
    })
}

/// `ip link set lo up` without iproute2: SIOCSIFFLAGS on an AF_INET socket.
fn loopback_up() {
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            return;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        let name = CString::new("lo").unwrap();
        std::ptr::copy_nonoverlapping(name.as_ptr(), ifr.ifr_name.as_mut_ptr(), 2);
        if libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) == 0 {
            ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr);
        }
        libc::close(sock);
    }
}

/// Fork. In the parent (stays PID 1): reap every child until the agent
/// child exits, then return its exit code. In the child: return `None` so
/// the caller continues as the agent.
pub fn fork_reaper() -> Option<i32> {
    match unsafe { fork() } {
        Ok(ForkResult::Child) => None,
        Ok(ForkResult::Parent { child }) => Some(reap_until(child)),
        Err(e) => {
            eprintln!("vmlayer-guest-agent: fork failed: {e}; running without a reaper");
            None
        }
    }
}

fn reap_until(agent: Pid) -> i32 {
    // Forward termination to the agent so a clean shutdown is possible.
    unsafe {
        extern "C" fn forward(_sig: libc::c_int) {}
        libc::signal(libc::SIGTERM, forward as *const () as usize);
        libc::signal(libc::SIGINT, forward as *const () as usize);
    }
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::empty())) {
            Ok(WaitStatus::Exited(pid, code)) if pid == agent => return code,
            Ok(WaitStatus::Signaled(pid, _, _)) if pid == agent => return 1,
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => {
                let _ = nix::sys::signal::kill(agent, nix::sys::signal::Signal::SIGTERM);
            }
            Err(nix::errno::Errno::ECHILD) => return 0,
            Err(_) => return 1,
        }
    }
}
