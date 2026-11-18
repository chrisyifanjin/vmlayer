//! Host networking for microVMs: one network namespace per VM.
//!
//! Every guest sees the same network (`172.16.0.2/30`, gateway `.1`, fixed
//! MAC), which is what makes restore and fork trivial: a snapshot carries
//! the guest's NIC state, and N restores of it do not collide because each
//! lives in its own namespace. The namespace is wired to the host with a
//! veth pair on a per-VM /30 from the pool; the host NATs egress and the
//! namespace DNATs its veth address to the guest for `expose`.
//!
//! ```text
//!   guest eth0 172.16.0.2 ── tap0 172.16.0.1 (netns vml-N) ── veth0 10.200.x.2
//!                                                              │
//!                                               host: vmlvethN 10.200.x.1 ── NAT ── world
//! ```
//!
//! Isolation (decision 0012): the host drops everything a VM sends to the
//! host itself except DNS to its proxy, and everything one VM sends to
//! another. Each namespace drops traffic to cloud metadata endpoints, and
//! with an egress policy, everything the policy does not allow.
//!
//! Uses the `ip` and `nft` commands rather than netlink bindings: they are
//! on every Linux host that can run Firecracker, and the commands double as
//! documentation of what the daemon does to the host.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use tokio::process::Command;
use vmlayer_core::{EgressPolicy, EgressRule, Error, Result, SandboxId};

use crate::api::err;

/// Fixed guest-side addressing, identical in every VM.
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(172, 16, 0, 2);
pub const GUEST_GW: Ipv4Addr = Ipv4Addr::new(172, 16, 0, 1);
pub const GUEST_MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 252);
pub const GUEST_MAC: &str = "06:00:ac:10:00:02";
pub const TAP: &str = "tap0";
/// Port of the per-VM DNS proxy on the host end of the veth pair.
pub const DNS_PORT: u16 = 53;
/// Never reachable from a guest: the link-local range that holds every major
/// cloud's instance metadata service (169.254.169.254), and Azure's
/// platform endpoint, and Alibaba's. Metadata hands out the host's cloud
/// credentials.
const METADATA: &str = "169.254.0.0/16, 168.63.129.16, 100.100.100.200";
const VETH_NS: &str = "veth0";

/// Host side address space for the veth pairs: one /30 per VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subnet {
    /// e.g. 10.200.0.0
    pub network: Ipv4Addr,
    /// prefix of the whole pool, 16 by default (16k VMs)
    pub prefix: u8,
}

impl Default for Subnet {
    fn default() -> Self {
        Self {
            network: Ipv4Addr::new(10, 200, 0, 0),
            prefix: 16,
        }
    }
}

impl Subnet {
    fn capacity(&self) -> u32 {
        (1u32 << (32 - self.prefix)) / 4
    }

    pub fn cidr(&self) -> String {
        format!("{}/{}", self.network, self.prefix)
    }
}

/// What one VM got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub index: u32,
    pub netns: String,
    /// Host end of the veth pair.
    pub veth_host: String,
    pub host_ip: Ipv4Addr,
    /// Namespace end; the address `expose` connects to.
    pub ns_ip: Ipv4Addr,
}

impl Lease {
    /// Static network config for the guest kernel (`ip=` as documented in
    /// Documentation/admin-guide/nfs/nfsroot.rst) plus vmlayer's own keys.
    pub fn kernel_args(dns: &[String]) -> String {
        let mut s = format!("ip={GUEST_IP}::{GUEST_GW}:{GUEST_MASK}::eth0:off");
        if !dns.is_empty() {
            s.push_str(&format!(" vmlayer.dns={}", dns.join(",")));
        }
        s
    }
}

/// Address allocator. After a daemon restart, leases are recovered from the
/// namespaces that exist (`vml-N`), so no duplicates are handed out.
pub struct Pool {
    subnet: Subnet,
    leases: BTreeMap<u32, SandboxId>,
    next: u32,
}

impl Pool {
    pub fn new(subnet: Subnet) -> Self {
        Self {
            subnet,
            leases: BTreeMap::new(),
            next: 0,
        }
    }

    pub fn lease_for(&self, index: u32) -> Lease {
        let base = u32::from(self.subnet.network) + index * 4;
        Lease {
            index,
            netns: format!("vml-{index}"),
            veth_host: format!("vmlveth{index}"),
            host_ip: Ipv4Addr::from(base + 1),
            ns_ip: Ipv4Addr::from(base + 2),
        }
    }

    pub fn allocate(&mut self, id: &SandboxId) -> Result<Lease> {
        let cap = self.subnet.capacity();
        for _ in 0..cap {
            let i = self.next % cap;
            self.next = self.next.wrapping_add(1);
            if let std::collections::btree_map::Entry::Vacant(v) = self.leases.entry(i) {
                v.insert(id.clone());
                return Ok(self.lease_for(i));
            }
        }
        Err(Error::ResourceExhausted(format!(
            "no free VM networks in {}",
            self.subnet.cidr()
        )))
    }

    pub fn claim(&mut self, index: u32, id: &SandboxId) -> Lease {
        self.leases.insert(index, id.clone());
        self.lease_for(index)
    }

    pub fn release(&mut self, index: u32) {
        self.leases.remove(&index);
    }
}

async fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| err(format!("running {cmd}: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

async fn ns(netns: &str, args: &[&str]) -> Result<String> {
    let mut full = vec!["netns", "exec", netns];
    full.extend_from_slice(args);
    run("ip", &full).await
}

async fn nft_apply(netns: Option<&str>, rules: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = match netns {
        Some(n) => {
            let mut c = Command::new("ip");
            c.args(["netns", "exec", n, "nft", "-f", "-"]);
            c
        }
        None => {
            let mut c = Command::new("nft");
            c.args(["-f", "-"]);
            c
        }
    };
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| err(format!("running nft: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(rules.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(err(format!(
            "nft: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Host-wide, idempotent: forwarding on, NAT for the whole pool.
pub async fn ensure_host(subnet: Subnet) -> Result<()> {
    let _ = tokio::fs::write("/proc/sys/net/ipv4/ip_forward", "1").await;
    let rules = host_rules(subnet);
    let _ = run("nft", &["delete", "table", "ip", "vmlayer"]).await;
    nft_apply(None, &rules).await?;
    allow_through_docker(&subnet.cidr()).await;
    Ok(())
}

/// Docker sets the host's FORWARD policy to drop, which would silently cut
/// microVM egress on any host that also runs Docker. `DOCKER-USER` is the
/// chain Docker reserves for the administrator's own rules and evaluates
/// first, so accept the pool's traffic there. A host without Docker (no such
/// chain) is left alone. Other firewalls with a forward-drop policy (ufw,
/// firewalld) need the equivalent rule from the administrator.
async fn allow_through_docker(cidr: &str) {
    if run("iptables", &["-w", "-n", "-L", "DOCKER-USER"])
        .await
        .is_err()
    {
        return;
    }
    for dir in ["-s", "-d"] {
        let rule = ["DOCKER-USER", dir, cidr, "-j", "ACCEPT"];
        let check = [&["-w", "-C"], &rule[..]].concat();
        if run("iptables", &check).await.is_ok() {
            continue;
        }
        let insert = [&["-w", "-I"], &rule[..]].concat();
        match run("iptables", &insert).await {
            Ok(_) => {
                tracing::info!(%cidr, dir, "allowed the sandbox pool through Docker's forward chain")
            }
            Err(error) => {
                tracing::warn!(%error, "could not add a DOCKER-USER rule; microVM egress may be blocked")
            }
        }
    }
}

/// Host table: NAT for the pool, and isolation of the host and of VMs from
/// each other. Replies (to `expose` connections, to the daemon) pass as
/// established traffic.
fn host_rules(subnet: Subnet) -> String {
    format!(
        r#"table ip vmlayer {{
  chain postrouting {{
    type nat hook postrouting priority srcnat; policy accept;
    ip saddr {pool} masquerade
  }}
  chain input {{
    type filter hook input priority filter; policy accept;
    iifname "vmlveth*" ct state established,related accept
    iifname "vmlveth*" meta l4proto {{ tcp, udp }} th dport {DNS_PORT} accept
    iifname "vmlveth*" counter drop
  }}
  chain forward {{
    type filter hook forward priority filter; policy accept;
    iifname "vmlveth*" oifname "vmlveth*" counter drop
  }}
}}
"#,
        pool = subnet.cidr()
    )
}

/// Namespace table: DNAT for `expose`, NAT out, metadata blocked, and the
/// egress policy if there is one. Addresses learned by the DNS proxy go into
/// the `dns_any` and `dns_port` sets.
pub(crate) fn netns_rules(lease: &Lease, egress: Option<&EgressPolicy>) -> String {
    let mut pre = vec![format!("ip daddr {} dnat to {GUEST_IP}", lease.ns_ip)];
    let mut fwd = vec![format!(
        "iifname \"{TAP}\" ip daddr {{ {METADATA} }} counter drop"
    )];
    if let Some(policy) = egress {
        let host = lease.host_ip;
        pre.push(format!(
            "iifname \"{TAP}\" meta l4proto {{ tcp, udp }} th dport 53 dnat to {host}:{DNS_PORT}"
        ));
        fwd.push(format!(
            "iifname \"{TAP}\" ct state established,related accept"
        ));
        fwd.push(format!(
            "iifname \"{TAP}\" ip daddr {host} meta l4proto {{ tcp, udp }} th dport {DNS_PORT} accept"
        ));
        for rule in &policy.allow {
            if let EgressRule::Network {
                addr,
                prefix,
                ports,
            } = rule
            {
                fwd.push(match ports.as_slice() {
                    [] => format!("iifname \"{TAP}\" ip daddr {addr}/{prefix} accept"),
                    ports => format!(
                        "iifname \"{TAP}\" ip daddr {addr}/{prefix} meta l4proto {{ tcp, udp }} th dport {{ {} }} accept",
                        ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
                    ),
                });
            }
        }
        fwd.push(format!("iifname \"{TAP}\" ip daddr @dns_any accept"));
        fwd.push(format!(
            "iifname \"{TAP}\" meta l4proto {{ tcp, udp }} ip daddr . th dport @dns_port accept"
        ));
        // Reject rather than drop: a blocked connection fails at once
        // instead of hanging until the client's timeout.
        fwd.push(format!(
            "iifname \"{TAP}\" meta l4proto tcp counter reject with tcp reset"
        ));
        fwd.push(format!(
            "iifname \"{TAP}\" counter reject with icmp type admin-prohibited"
        ));
    }
    let indent = |lines: Vec<String>| {
        lines
            .into_iter()
            .map(|l| format!("    {l}\n"))
            .collect::<String>()
    };
    format!(
        r#"table ip vmlayer {{
  set dns_any {{
    type ipv4_addr; flags timeout;
  }}
  set dns_port {{
    type ipv4_addr . inet_service; flags timeout;
  }}
  chain prerouting {{
    type nat hook prerouting priority dstnat; policy accept;
{pre}  }}
  chain postrouting {{
    type nat hook postrouting priority srcnat; policy accept;
    ip saddr {GUEST_IP} oifname "{VETH_NS}" masquerade
  }}
  chain forward {{
    type filter hook forward priority filter; policy accept;
{fwd}  }}
}}
"#,
        pre = indent(pre),
        fwd = indent(fwd),
    )
}

/// Let `entries` through the namespace's firewall (from the DNS proxy).
pub(crate) async fn allow_resolved(netns: &str, entries: &[crate::dns::Allowed]) -> Result<()> {
    let mut any = Vec::new();
    let mut port = Vec::new();
    for e in entries {
        match e.port {
            None => any.push(format!("{} timeout {}s", e.addr, e.ttl)),
            Some(p) => port.push(format!("{} . {p} timeout {}s", e.addr, e.ttl)),
        }
    }
    let mut rules = String::new();
    if !any.is_empty() {
        rules.push_str(&format!(
            "add element ip vmlayer dns_any {{ {} }}\n",
            any.join(", ")
        ));
    }
    if !port.is_empty() {
        rules.push_str(&format!(
            "add element ip vmlayer dns_port {{ {} }}\n",
            port.join(", ")
        ));
    }
    nft_apply(Some(netns), &rules).await
}

/// Build the VM's namespace: tap for Firecracker, veth to the host, NAT out,
/// DNAT in for `expose`, and the firewall for `egress`. Owned by the jail uid
/// so Firecracker can open the tap.
pub async fn create_netns(
    lease: &Lease,
    uid: u32,
    gid: u32,
    egress: Option<&EgressPolicy>,
) -> Result<()> {
    let _ = run("ip", &["netns", "del", &lease.netns]).await;
    run("ip", &["netns", "add", &lease.netns]).await?;
    let veth_ns = VETH_NS;
    run(
        "ip",
        &[
            "link",
            "add",
            &lease.veth_host,
            "type",
            "veth",
            "peer",
            "name",
            veth_ns,
            "netns",
            &lease.netns,
        ],
    )
    .await?;
    run(
        "ip",
        &[
            "addr",
            "add",
            &format!("{}/30", lease.host_ip),
            "dev",
            &lease.veth_host,
        ],
    )
    .await?;
    run("ip", &["link", "set", &lease.veth_host, "up"]).await?;
    ns(
        &lease.netns,
        &[
            "ip",
            "addr",
            "add",
            &format!("{}/30", lease.ns_ip),
            "dev",
            veth_ns,
        ],
    )
    .await?;
    ns(&lease.netns, &["ip", "link", "set", veth_ns, "up"]).await?;
    ns(&lease.netns, &["ip", "link", "set", "lo", "up"]).await?;
    ns(
        &lease.netns,
        &[
            "ip",
            "route",
            "add",
            "default",
            "via",
            &lease.host_ip.to_string(),
        ],
    )
    .await?;
    ns(
        &lease.netns,
        &[
            "ip",
            "tuntap",
            "add",
            "dev",
            TAP,
            "mode",
            "tap",
            "user",
            &uid.to_string(),
            "group",
            &gid.to_string(),
        ],
    )
    .await?;
    ns(
        &lease.netns,
        &["ip", "addr", "add", &format!("{GUEST_GW}/30"), "dev", TAP],
    )
    .await?;
    ns(&lease.netns, &["ip", "link", "set", TAP, "up"]).await?;
    ns(&lease.netns, &["sysctl", "-qw", "net.ipv4.ip_forward=1"]).await?;
    nft_apply(Some(&lease.netns), &netns_rules(lease, egress)).await?;
    Ok(())
}

pub async fn delete_netns(lease: &Lease) {
    let _ = run("ip", &["link", "del", &lease.veth_host]).await;
    let _ = run("ip", &["netns", "del", &lease.netns]).await;
}

/// Namespaces that exist on the host, by index, for lease recovery.
pub async fn existing_netns() -> Vec<u32> {
    let out = run("ip", &["netns", "list"]).await.unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            l.split_whitespace()
                .next()?
                .strip_prefix("vml-")?
                .parse()
                .ok()
        })
        .collect()
}

/// A loopback-bound TCP proxy to the guest (via the namespace's DNAT).
pub struct Proxy {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn proxy(target: Ipv4Addr, port: u16) -> Result<Proxy> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let host_port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                if let Ok(mut outbound) = tokio::net::TcpStream::connect((target, port)).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    Ok(Proxy {
        port: host_port,
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_are_disjoint_slash30s() {
        let p = Pool::new(Subnet::default());
        let a = p.lease_for(0);
        let b = p.lease_for(1);
        assert_eq!(
            (a.host_ip, a.ns_ip),
            (Ipv4Addr::new(10, 200, 0, 1), Ipv4Addr::new(10, 200, 0, 2))
        );
        assert_eq!(
            (b.host_ip, b.ns_ip),
            (Ipv4Addr::new(10, 200, 0, 5), Ipv4Addr::new(10, 200, 0, 6))
        );
        assert_eq!(a.netns, "vml-0");
        assert_eq!(p.lease_for(70).host_ip, Ipv4Addr::new(10, 200, 1, 25));
    }

    #[test]
    fn pool_allocates_releases_and_recovers() {
        let mut p = Pool::new(Subnet::default());
        let a = p.allocate(&SandboxId("a".into())).unwrap();
        let b = p.allocate(&SandboxId("b".into())).unwrap();
        assert_eq!((a.index, b.index), (0, 1));
        p.release(0);
        let c = p.claim(0, &SandboxId("c".into()));
        assert_eq!(c.index, 0);
        assert_eq!(p.allocate(&SandboxId("d".into())).unwrap().index, 2);
    }

    #[test]
    fn kernel_ip_arg() {
        assert_eq!(
            Lease::kernel_args(&["1.1.1.1".into(), "8.8.8.8".into()]),
            "ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off vmlayer.dns=1.1.1.1,8.8.8.8"
        );
    }

    #[tokio::test]
    async fn proxy_forwards_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bport = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = backend.accept().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(b"pong").await.unwrap();
        });
        let p = proxy(Ipv4Addr::LOCALHOST, bport).await.unwrap();
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", p.port))
            .await
            .unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut out = [0u8; 4];
        c.read_exact(&mut out).await.unwrap();
        assert_eq!(&out, b"pong");
    }
}

#[cfg(test)]
mod rule_tests {
    use super::*;

    fn lease() -> Lease {
        Pool::new(Subnet::default()).lease_for(1)
    }

    #[test]
    fn without_a_policy_only_metadata_is_blocked() {
        let r = netns_rules(&lease(), None);
        assert!(r.contains("ip daddr 10.200.0.6 dnat to 172.16.0.2"), "{r}");
        assert!(
            r.contains("ip daddr { 169.254.0.0/16, 168.63.129.16, 100.100.100.200 } counter drop"),
            "{r}"
        );
        assert!(!r.contains("reject"), "{r}");
        assert!(!r.contains("dport 53 dnat"), "{r}");
    }

    #[test]
    fn a_policy_redirects_dns_and_rejects_the_rest() {
        let policy = EgressPolicy::parse(&["10.0.0.0/8", "1.2.3.4:443,80", "pypi.org"]).unwrap();
        let r = netns_rules(&lease(), Some(&policy));
        for needle in [
            "th dport 53 dnat to 10.200.0.5:53",
            "ip daddr 10.200.0.5 meta l4proto { tcp, udp } th dport 53 accept",
            "ip daddr 10.0.0.0/8 accept",
            "ip daddr 1.2.3.4/32 meta l4proto { tcp, udp } th dport { 443, 80 } accept",
            "ip daddr @dns_any accept",
            "ip daddr . th dport @dns_port accept",
            "reject with tcp reset",
        ] {
            assert!(r.contains(needle), "missing {needle:?} in\n{r}");
        }
        // Metadata is dropped before anything can accept it.
        let metadata = r.find("168.63.129.16").unwrap();
        let first_accept = r.find(" accept\n").unwrap();
        assert!(metadata < first_accept, "{r}");
    }

    #[test]
    fn host_rules_isolate_the_host_and_other_vms() {
        let r = host_rules(Subnet::default());
        assert!(r.contains("ip saddr 10.200.0.0/16 masquerade"));
        assert!(r.contains("iifname \"vmlveth*\" counter drop"));
        assert!(r.contains("iifname \"vmlveth*\" oifname \"vmlveth*\" counter drop"));
    }
}
