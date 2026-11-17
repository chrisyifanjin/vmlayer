//! Per-sandbox DNS proxy that enforces the domain rules of an egress
//! allowlist (decision 0012).
//!
//! The VM's namespace redirects all guest DNS (any resolver, port 53, UDP or
//! TCP) to this proxy on the host end of the VM's veth pair. The proxy
//! answers NXDOMAIN for names the policy does not allow. For allowed names it
//! forwards the query upstream and, before replying, adds every IPv4 address
//! in the answer to the namespace's nftables sets with the record's TTL, so
//! the connection that follows the lookup is let through. This is the
//! approach of Cilium's FQDN policies and dnsmasq's `--nftset`: the allowed
//! addresses follow DNS instead of being frozen at create time.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::RData;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use vmlayer_core::{EgressPolicy, Result};

use crate::api::err;

/// How long an address learned from DNS stays allowed, at least and at most.
/// The floor covers clients that cache answers past their TTL.
const MIN_ALLOW: u32 = 300;
const MAX_ALLOW: u32 = 86_400;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(3);
const TCP_IDLE: Duration = Duration::from_secs(10);
const MAX_INFLIGHT: usize = 64;

/// An address (and port, or every port) to let through for `ttl` seconds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Allowed {
    pub(crate) addr: Ipv4Addr,
    pub(crate) port: Option<u16>,
    pub(crate) ttl: u32,
}

/// Where learned addresses go: the VM namespace's nftables sets.
#[async_trait]
pub(crate) trait AllowSink: Send + Sync + 'static {
    async fn allow(&self, entries: &[Allowed]) -> Result<()>;
}

/// Decides and forwards; transport-independent so it can be unit tested.
pub(crate) struct Resolver {
    policy: EgressPolicy,
    upstreams: Vec<SocketAddr>,
    sink: Arc<dyn AllowSink>,
}

impl Resolver {
    pub(crate) fn new(
        policy: EgressPolicy,
        upstreams: Vec<SocketAddr>,
        sink: Arc<dyn AllowSink>,
    ) -> Self {
        Self {
            policy,
            upstreams,
            sink,
        }
    }

    /// The reply to send for `query`, or `None` to stay silent (garbage in).
    pub(crate) async fn answer(&self, query: &[u8], tcp: bool) -> Option<Vec<u8>> {
        let msg = Message::from_vec(query).ok()?;
        if msg.metadata.message_type != MessageType::Query {
            return None;
        }
        let Some(question) = msg.queries.first() else {
            return reply(&msg, ResponseCode::FormErr);
        };
        let name = question.name.to_ascii();
        let rules: Vec<_> = self.policy.domain_rules(&name).collect();
        if rules.is_empty() {
            tracing::debug!(%name, "DNS query denied by the egress policy");
            return reply(&msg, ResponseCode::NXDomain);
        }
        let Some(response) = self.forward(query, msg.metadata.id, tcp).await else {
            return reply(&msg, ResponseCode::ServFail);
        };
        let Ok(parsed) = Message::from_vec(&response) else {
            return reply(&msg, ResponseCode::ServFail);
        };
        let mut entries = BTreeSet::new();
        for record in &parsed.answers {
            if let RData::A(a) = &record.data {
                let ttl = record.ttl.clamp(MIN_ALLOW, MAX_ALLOW);
                for rule in &rules {
                    if rule.ports().is_empty() {
                        entries.insert(Allowed {
                            addr: a.0,
                            port: None,
                            ttl,
                        });
                    }
                    for &port in rule.ports() {
                        entries.insert(Allowed {
                            addr: a.0,
                            port: Some(port),
                            ttl,
                        });
                    }
                }
            }
        }
        if !entries.is_empty() {
            let entries: Vec<_> = entries.into_iter().collect();
            if let Err(error) = self.sink.allow(&entries).await {
                // Answering would hand out addresses the guest cannot reach.
                tracing::warn!(%name, %error, "could not allow resolved addresses");
                return reply(&msg, ResponseCode::ServFail);
            }
        }
        Some(response)
    }

    async fn forward(&self, query: &[u8], id: u16, tcp: bool) -> Option<Vec<u8>> {
        for upstream in &self.upstreams {
            let attempt = async {
                if tcp {
                    let mut stream = TcpStream::connect(upstream).await.ok()?;
                    stream.write_u16(query.len() as u16).await.ok()?;
                    stream.write_all(query).await.ok()?;
                    let len = stream.read_u16().await.ok()?;
                    let mut buf = vec![0; len as usize];
                    stream.read_exact(&mut buf).await.ok()?;
                    Some(buf)
                } else {
                    let socket = UdpSocket::bind(("0.0.0.0", 0)).await.ok()?;
                    socket.send_to(query, upstream).await.ok()?;
                    let mut buf = vec![0; 65_535];
                    loop {
                        let (n, from) = socket.recv_from(&mut buf).await.ok()?;
                        // Ignore anything that is not the upstream's reply.
                        if from == *upstream && n >= 2 && u16::from_be_bytes([buf[0], buf[1]]) == id
                        {
                            buf.truncate(n);
                            return Some(buf);
                        }
                    }
                }
            };
            if let Ok(Some(reply)) = tokio::time::timeout(UPSTREAM_TIMEOUT, attempt).await {
                return Some(reply);
            }
            tracing::debug!(%upstream, "upstream resolver did not answer");
        }
        None
    }
}

fn reply(query: &Message, code: ResponseCode) -> Option<Vec<u8>> {
    let mut msg = Message::error_msg(query.metadata.id, query.metadata.op_code, code);
    msg.metadata.recursion_desired = query.metadata.recursion_desired;
    msg.metadata.recursion_available = true;
    msg.add_queries(query.queries.iter().cloned());
    msg.to_vec().ok()
}

/// The running proxy. Dropping it stops it.
pub(crate) struct DnsProxy {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for DnsProxy {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl DnsProxy {
    /// Serve UDP and TCP DNS on `listen`.
    pub(crate) async fn start(listen: SocketAddr, resolver: Resolver) -> Result<Self> {
        let resolver = Arc::new(resolver);
        let limit = Arc::new(Semaphore::new(MAX_INFLIGHT));
        let udp = Arc::new(
            UdpSocket::bind(listen)
                .await
                .map_err(|e| err(format!("DNS proxy on {listen} (udp): {e}")))?,
        );
        let tcp = TcpListener::bind(listen)
            .await
            .map_err(|e| err(format!("DNS proxy on {listen} (tcp): {e}")))?;

        let udp_task = {
            let (resolver, limit) = (resolver.clone(), limit.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    let Ok((n, peer)) = udp.recv_from(&mut buf).await else {
                        continue;
                    };
                    let Ok(permit) = limit.clone().try_acquire_owned() else {
                        continue; // overloaded: the client retries
                    };
                    let query = buf[..n].to_vec();
                    let (udp, resolver) = (udp.clone(), resolver.clone());
                    tokio::spawn(async move {
                        if let Some(answer) = resolver.answer(&query, false).await {
                            let _ = udp.send_to(&answer, peer).await;
                        }
                        drop(permit);
                    });
                }
            })
        };
        let tcp_task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = tcp.accept().await else {
                    continue;
                };
                let Ok(permit) = limit.clone().try_acquire_owned() else {
                    continue;
                };
                let resolver = resolver.clone();
                tokio::spawn(async move {
                    let serve = async {
                        loop {
                            let len = stream.read_u16().await.ok()?;
                            let mut query = vec![0; len as usize];
                            stream.read_exact(&mut query).await.ok()?;
                            let answer = resolver.answer(&query, true).await?;
                            stream.write_u16(answer.len() as u16).await.ok()?;
                            stream.write_all(&answer).await.ok()?;
                        }
                        #[allow(unreachable_code)]
                        Some(())
                    };
                    let _ = tokio::time::timeout(TCP_IDLE, serve).await;
                    drop(permit);
                });
            }
        });
        Ok(Self {
            tasks: vec![udp_task, tcp_task],
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hickory_proto::op::Query;
    use hickory_proto::rr::rdata::A;
    use hickory_proto::rr::{Name, Record, RecordType};

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Allowed>>);

    #[async_trait]
    impl AllowSink for Recorder {
        async fn allow(&self, entries: &[Allowed]) -> Result<()> {
            self.0.lock().unwrap().extend_from_slice(entries);
            Ok(())
        }
    }

    fn query(name: &str) -> Vec<u8> {
        let mut m = Message::query();
        m.metadata.recursion_desired = true;
        m.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
        m.to_vec().unwrap()
    }

    /// A one-shot upstream that answers every query with two A records.
    async fn fake_upstream() -> SocketAddr {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0; 4096];
            loop {
                let (n, peer) = socket.recv_from(&mut buf).await.unwrap();
                let q = Message::from_vec(&buf[..n]).unwrap();
                let mut r = Message::response(q.metadata.id, q.metadata.op_code);
                r.add_queries(q.queries.iter().cloned());
                let name = q.queries[0].name.clone();
                r.add_answer(Record::from_rdata(
                    name.clone(),
                    20,
                    RData::A(A::new(1, 2, 3, 4)),
                ));
                r.add_answer(Record::from_rdata(
                    name,
                    9_000,
                    RData::A(A::new(5, 6, 7, 8)),
                ));
                socket.send_to(&r.to_vec().unwrap(), peer).await.unwrap();
            }
        });
        addr
    }

    #[tokio::test]
    async fn allowed_names_resolve_and_open_their_addresses() {
        let sink = Arc::new(Recorder::default());
        let policy = EgressPolicy::parse(&["pypi.org", "*.pythonhosted.org:443,80"]).unwrap();
        let resolver = Resolver::new(policy, vec![fake_upstream().await], sink.clone());

        let denied = resolver
            .answer(&query("example.com."), false)
            .await
            .unwrap();
        let denied = Message::from_vec(&denied).unwrap();
        assert_eq!(denied.metadata.response_code, ResponseCode::NXDomain);
        assert!(sink.0.lock().unwrap().is_empty());

        let ok = resolver.answer(&query("pypi.org."), false).await.unwrap();
        let ok = Message::from_vec(&ok).unwrap();
        assert_eq!(
            (ok.metadata.response_code, ok.answers.len()),
            (ResponseCode::NoError, 2)
        );
        let got = std::mem::take(&mut *sink.0.lock().unwrap());
        assert_eq!(
            got,
            vec![
                Allowed {
                    addr: Ipv4Addr::new(1, 2, 3, 4),
                    port: None,
                    ttl: MIN_ALLOW
                },
                Allowed {
                    addr: Ipv4Addr::new(5, 6, 7, 8),
                    port: None,
                    ttl: 9_000
                },
            ]
        );

        resolver
            .answer(&query("files.pythonhosted.org."), false)
            .await
            .unwrap();
        let ports: Vec<_> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|a| (a.addr, a.port))
            .collect();
        assert_eq!(ports.len(), 4);
        assert!(ports.contains(&(Ipv4Addr::new(1, 2, 3, 4), Some(443))));
        assert!(ports.contains(&(Ipv4Addr::new(5, 6, 7, 8), Some(80))));
    }

    #[tokio::test]
    async fn unreachable_upstream_is_servfail_and_garbage_is_ignored() {
        let policy = EgressPolicy::parse(&["pypi.org"]).unwrap();
        // Nothing listens on this port.
        let dead = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let resolver = Resolver::new(policy, vec![dead], Arc::new(Recorder::default()));
        let r = resolver.answer(&query("pypi.org."), false).await.unwrap();
        assert_eq!(
            Message::from_vec(&r).unwrap().metadata.response_code,
            ResponseCode::ServFail
        );
        assert!(resolver.answer(b"\x00\x01garbage", false).await.is_none());
    }

    #[tokio::test]
    async fn serves_udp_and_tcp() {
        let policy = EgressPolicy::parse(&["pypi.org"]).unwrap();
        let resolver = Resolver::new(
            policy,
            vec![fake_upstream().await],
            Arc::new(Recorder::default()),
        );
        let port = TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let listen: SocketAddr = ([127, 0, 0, 1], port).into();
        let proxy = DnsProxy::start(listen, resolver).await.unwrap();

        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        client
            .send_to(&query("nope.example."), listen)
            .await
            .unwrap();
        let mut buf = vec![0; 4096];
        let n = client.recv(&mut buf).await.unwrap();
        assert_eq!(
            Message::from_vec(&buf[..n]).unwrap().metadata.response_code,
            ResponseCode::NXDomain
        );

        let mut tcp = TcpStream::connect(listen).await.unwrap();
        let q = query("pypi.org.");
        tcp.write_u16(q.len() as u16).await.unwrap();
        tcp.write_all(&q).await.unwrap();
        let len = tcp.read_u16().await.unwrap();
        let mut answer = vec![0; len as usize];
        tcp.read_exact(&mut answer).await.unwrap();
        // The fake upstream speaks UDP only, so the TCP path reports SERVFAIL.
        assert_eq!(
            Message::from_vec(&answer).unwrap().metadata.response_code,
            ResponseCode::ServFail
        );
        drop(proxy);
    }
}
