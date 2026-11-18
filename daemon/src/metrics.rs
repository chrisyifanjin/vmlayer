//! Prometheus metrics and per-request tracing (decision 0007).
//!
//! `[metrics] listen` serves `GET /metrics` in the Prometheus text format.
//! Counters and histograms are recorded where things happen; gauges (live
//! sandboxes, allocated and allocatable resources) are read at scrape time.
//! Every RPC runs in a `tracing` span carrying its method and a request ID,
//! which is returned to the caller in `x-request-id`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use http::{HeaderValue, Response, StatusCode};
use http_body_util::Full;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::manager::Daemon;

/// `[metrics]` in the daemon config.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Address for `GET /metrics`, e.g. `127.0.0.1:9464`. Unset: off.
    pub listen: Option<String>,
}

/// Seconds; covers a fast exec (milliseconds) to a slow cold boot.
const BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

const HELP: &[(&str, &str, &str)] = &[
    (
        "vmlayer_rpc_requests_total",
        "counter",
        "gRPC requests by method and status code.",
    ),
    (
        "vmlayer_rpc_duration_seconds",
        "histogram",
        "gRPC request time to response headers, by method.",
    ),
    (
        "vmlayer_sandbox_operations_total",
        "counter",
        "Sandbox creates, restores, forks and kills by backend and result.",
    ),
    (
        "vmlayer_sandbox_operation_duration_seconds",
        "histogram",
        "Time to create, restore or fork a sandbox, by backend.",
    ),
    (
        "vmlayer_admission_refusals_total",
        "counter",
        "Sandboxes refused because the host was full.",
    ),
    (
        "vmlayer_gateway_requests_total",
        "counter",
        "Port gateway requests by HTTP status.",
    ),
    (
        "vmlayer_credential_proxy_requests_total",
        "counter",
        "Credential proxy requests by credential and HTTP status.",
    ),
    (
        "vmlayer_sandboxes",
        "gauge",
        "Live sandboxes on this host, by backend.",
    ),
    (
        "vmlayer_allocated_vcpus",
        "gauge",
        "vCPUs of live sandboxes.",
    ),
    (
        "vmlayer_allocated_memory_mib",
        "gauge",
        "Memory of live sandboxes, MiB.",
    ),
    (
        "vmlayer_allocatable_vcpus",
        "gauge",
        "vCPUs admission control allows in total (0: unlimited).",
    ),
    (
        "vmlayer_allocatable_memory_mib",
        "gauge",
        "Memory admission control allows in total, MiB (0: unlimited).",
    ),
    ("vmlayer_build_info", "gauge", "Daemon version."),
];

type Labels = Vec<(&'static str, String)>;
type Key = (&'static str, Labels);
/// A gauge read at scrape time: name, labels, value.
pub type Gauge = (&'static str, Labels, f64);

#[derive(Default)]
struct Hist {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

#[derive(Default)]
struct Inner {
    counters: BTreeMap<Key, u64>,
    hists: BTreeMap<Key, Hist>,
}

#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

fn key(name: &'static str, labels: &[(&'static str, &str)]) -> Key {
    (
        name,
        labels.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
    )
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn labels_text(labels: &[(&'static str, String)], extra: Option<(&str, String)>) -> String {
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{v}\""));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

impl Metrics {
    pub fn inc(&self, name: &'static str, labels: &[(&'static str, &str)]) {
        *self
            .inner
            .lock()
            .unwrap()
            .counters
            .entry(key(name, labels))
            .or_default() += 1;
    }

    pub fn observe(&self, name: &'static str, labels: &[(&'static str, &str)], seconds: f64) {
        let mut inner = self.inner.lock().unwrap();
        let h = inner.hists.entry(key(name, labels)).or_default();
        if h.counts.is_empty() {
            h.counts = vec![0; BUCKETS.len()];
        }
        for (i, b) in BUCKETS.iter().enumerate() {
            if seconds <= *b {
                h.counts[i] += 1;
            }
        }
        h.sum += seconds;
        h.count += 1;
    }

    /// Record a sandbox operation's result and, on success, its duration.
    pub fn operation(&self, op: &str, backend: &str, started: Instant, ok: bool) {
        let result = if ok { "ok" } else { "error" };
        self.inc(
            "vmlayer_sandbox_operations_total",
            &[("op", op), ("backend", backend), ("result", result)],
        );
        if ok {
            self.observe(
                "vmlayer_sandbox_operation_duration_seconds",
                &[("op", op), ("backend", backend)],
                started.elapsed().as_secs_f64(),
            );
        }
    }

    /// The text exposition, with `gauges` read by the caller.
    pub fn render(&self, gauges: &[Gauge]) -> String {
        let inner = self.inner.lock().unwrap();
        let mut by_name: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for ((name, labels), v) in &inner.counters {
            by_name
                .entry(name)
                .or_default()
                .push(format!("{name}{} {v}", labels_text(labels, None)));
        }
        for ((name, labels), h) in &inner.hists {
            let lines = by_name.entry(name).or_default();
            for (b, c) in BUCKETS.iter().zip(&h.counts) {
                lines.push(format!(
                    "{name}_bucket{} {c}",
                    labels_text(labels, Some(("le", b.to_string())))
                ));
            }
            lines.push(format!(
                "{name}_bucket{} {}",
                labels_text(labels, Some(("le", "+Inf".into()))),
                h.count
            ));
            lines.push(format!("{name}_sum{} {}", labels_text(labels, None), h.sum));
            lines.push(format!(
                "{name}_count{} {}",
                labels_text(labels, None),
                h.count
            ));
        }
        for (name, labels, v) in gauges {
            by_name
                .entry(name)
                .or_default()
                .push(format!("{name}{} {v}", labels_text(labels, None)));
        }
        let mut out = String::new();
        for (name, kind, help) in HELP {
            let Some(lines) = by_name.get(name) else {
                continue;
            };
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for l in lines {
                let _ = writeln!(out, "{l}");
            }
        }
        out
    }
}

/// The current gauges of `daemon`.
async fn gauges(daemon: &Daemon) -> Vec<Gauge> {
    let mut per_backend: BTreeMap<String, f64> = BTreeMap::new();
    for (name, _) in daemon.registry.entries() {
        per_backend.insert(name.clone(), 0.0);
    }
    for backend in daemon.live_backends().await {
        *per_backend.entry(backend).or_default() += 1.0;
    }
    let (used_cpu, used_mem) = daemon.allocated().await;
    let (cap_cpu, cap_mem) = daemon.allocatable();
    let mut g: Vec<_> = per_backend
        .into_iter()
        .map(|(b, n)| ("vmlayer_sandboxes", vec![("backend", b)], n))
        .collect();
    g.push(("vmlayer_allocated_vcpus", vec![], f64::from(used_cpu)));
    g.push(("vmlayer_allocated_memory_mib", vec![], used_mem as f64));
    g.push(("vmlayer_allocatable_vcpus", vec![], f64::from(cap_cpu)));
    g.push(("vmlayer_allocatable_memory_mib", vec![], cap_mem as f64));
    g.push((
        "vmlayer_build_info",
        vec![("version", crate::VERSION.to_string())],
        1.0,
    ));
    g
}

/// Serve `GET /metrics` until the process exits.
pub async fn serve(daemon: Arc<Daemon>) -> anyhow::Result<()> {
    let Some(listen) = daemon.cfg.metrics.listen.clone() else {
        return Ok(());
    };
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding the metrics endpoint to {listen}"))?;
    tracing::info!(%listen, "metrics endpoint listening");
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let service =
                hyper::service::service_fn(move |req: http::Request<hyper::body::Incoming>| {
                    let daemon = daemon.clone();
                    async move {
                        let resp = if req.uri().path() == "/metrics" {
                            let body = daemon.metrics.render(&gauges(&daemon).await);
                            let mut r = Response::new(Full::new(Bytes::from(body)));
                            r.headers_mut().insert(
                                http::header::CONTENT_TYPE,
                                HeaderValue::from_static("text/plain; version=0.0.4"),
                            );
                            r
                        } else {
                            let mut r =
                                Response::new(Full::new(Bytes::from_static(b"see /metrics\n")));
                            *r.status_mut() = StatusCode::NOT_FOUND;
                            r
                        };
                        Ok::<_, std::convert::Infallible>(resp)
                    }
                });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Middleware for every listener: a span per RPC with its method and a
/// request ID (the caller's `x-request-id`, or a new one), the ID echoed in
/// the response, and request counts and timings by method and status.
pub async fn trace(
    axum::extract::State(metrics): axum::extract::State<Arc<Metrics>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req
        .uri()
        .path()
        .strip_prefix("/vmlayer.v1.")
        .unwrap_or(req.uri().path())
        .to_string();
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::now_v7().simple().to_string());
    let span = tracing::info_span!("rpc", method = %method, request_id = %id);
    let started = Instant::now();
    let mut resp = next.run(req).instrument(span.clone()).await;
    // An error before any response body carries grpc-status in the headers;
    // a success carries it in the trailers, which this layer does not see.
    let code = resp
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0")
        .to_string();
    let seconds = started.elapsed().as_secs_f64();
    metrics.inc(
        "vmlayer_rpc_requests_total",
        &[("method", &method), ("code", &code)],
    );
    metrics.observe(
        "vmlayer_rpc_duration_seconds",
        &[("method", &method)],
        seconds,
    );
    span.in_scope(|| tracing::debug!(code = %code, ms = (seconds * 1000.0) as u64, "rpc done"));
    if let Ok(v) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_format() {
        let m = Metrics::default();
        m.inc(
            "vmlayer_rpc_requests_total",
            &[("method", "Sandboxes/Create"), ("code", "0")],
        );
        m.inc(
            "vmlayer_rpc_requests_total",
            &[("method", "Sandboxes/Create"), ("code", "0")],
        );
        m.observe(
            "vmlayer_rpc_duration_seconds",
            &[("method", "Sandboxes/Create")],
            0.3,
        );
        let text = m.render(&[("vmlayer_allocated_vcpus", vec![], 2.0)]);
        assert!(
            text.contains("# TYPE vmlayer_rpc_requests_total counter"),
            "{text}"
        );
        assert!(
            text.contains(r#"vmlayer_rpc_requests_total{method="Sandboxes/Create",code="0"} 2"#)
        );
        assert!(text.contains(
            r#"vmlayer_rpc_duration_seconds_bucket{method="Sandboxes/Create",le="0.25"} 0"#
        ));
        assert!(text.contains(
            r#"vmlayer_rpc_duration_seconds_bucket{method="Sandboxes/Create",le="0.5"} 1"#
        ));
        assert!(text.contains(
            r#"vmlayer_rpc_duration_seconds_bucket{method="Sandboxes/Create",le="+Inf"} 1"#
        ));
        assert!(text.contains("vmlayer_allocated_vcpus 2"));
        assert!(
            !text.contains("vmlayer_gateway_requests_total"),
            "unused metrics are omitted"
        );
    }
}
