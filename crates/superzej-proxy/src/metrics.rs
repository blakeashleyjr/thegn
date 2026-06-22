//! Minimal, dependency-free Prometheus text exposition — the same hand-rendered
//! approach as the Go `metrics.go` (no Prometheus SDK). Milestone 1 tracks the
//! headline counters; labelled per-backend families can be layered on later.

use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct Metrics {
    // Keyed by label tuple rendered as a stable string, value = counter.
    requests: Mutex<BTreeMap<String, u64>>,
    backend_attempts: Mutex<BTreeMap<String, u64>>,
    fallthroughs: Mutex<BTreeMap<String, u64>>,
    tokens: Mutex<BTreeMap<String, u64>>,
    cost_micros: Mutex<BTreeMap<String, u64>>, // USD * 1e6 to stay integer
    tokens_saved: Mutex<BTreeMap<String, u64>>,
}

fn bump(map: &Mutex<BTreeMap<String, u64>>, label: String, by: u64) {
    *map.lock().unwrap().entry(label).or_insert(0) += by;
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc_request(&self, route: &str, backend: &str, outcome: &str) {
        bump(
            &self.requests,
            format!("route=\"{route}\",backend=\"{backend}\",outcome=\"{outcome}\""),
            1,
        );
    }

    pub fn inc_backend_attempt(&self, backend: &str, outcome: &str) {
        bump(
            &self.backend_attempts,
            format!("backend=\"{backend}\",outcome=\"{outcome}\""),
            1,
        );
    }

    pub fn inc_fallthrough(&self, backend: &str, reason: &str) {
        bump(
            &self.fallthroughs,
            format!("backend=\"{backend}\",reason=\"{reason}\""),
            1,
        );
    }

    pub fn add_tokens(&self, backend: &str, kind: &str, n: u64) {
        if n > 0 {
            bump(
                &self.tokens,
                format!("backend=\"{backend}\",type=\"{kind}\""),
                n,
            );
        }
    }

    pub fn add_cost(&self, backend: &str, source: &str, usd: f64) {
        if usd > 0.0 {
            bump(
                &self.cost_micros,
                format!("backend=\"{backend}\",source=\"{source}\""),
                (usd * 1e6) as u64,
            );
        }
    }

    /// Records estimated tokens removed by in-flight compression (group W).
    pub fn add_tokens_saved(&self, backend: &str, n: u64) {
        if n > 0 {
            bump(&self.tokens_saved, format!("backend=\"{backend}\""), n);
        }
    }

    /// Renders all families in Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        render_counter(
            &mut out,
            "model_proxy_requests_total",
            "Completed client requests.",
            &self.requests,
            1.0,
        );
        render_counter(
            &mut out,
            "model_proxy_backend_attempts_total",
            "Backend attempts per outcome.",
            &self.backend_attempts,
            1.0,
        );
        render_counter(
            &mut out,
            "model_proxy_fallthroughs_total",
            "Fall-throughs per reason.",
            &self.fallthroughs,
            1.0,
        );
        render_counter(
            &mut out,
            "model_proxy_tokens_total",
            "Tokens observed.",
            &self.tokens,
            1.0,
        );
        render_counter(
            &mut out,
            "model_proxy_cost_usd_total",
            "Estimated spend in USD.",
            &self.cost_micros,
            1e-6,
        );
        render_counter(
            &mut out,
            "model_proxy_tokens_saved_total",
            "Estimated tokens removed by in-flight compression.",
            &self.tokens_saved,
            1.0,
        );
        out
    }
}

fn render_counter(
    out: &mut String,
    name: &str,
    help: &str,
    map: &Mutex<BTreeMap<String, u64>>,
    scale: f64,
) {
    let map = map.lock().unwrap();
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
    if map.is_empty() {
        out.push_str(&format!("{name} 0\n"));
        return;
    }
    for (labels, v) in map.iter() {
        if scale == 1.0 {
            out.push_str(&format!("{name}{{{labels}}} {v}\n"));
        } else {
            out.push_str(&format!("{name}{{{labels}}} {}\n", *v as f64 * scale));
        }
    }
}
