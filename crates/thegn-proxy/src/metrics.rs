//! Minimal, dependency-free Prometheus text exposition — the same hand-rendered
//! approach as the Go `metrics.go` (no Prometheus SDK). Milestone 1 tracks the
//! headline counters; labelled per-backend families can be layered on later.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Cumulative-histogram bucket upper bounds for request duration, in millis
/// (rendered as seconds — the same spread as the Go proxy's
/// `model_proxy_request_duration_seconds`).
const DURATION_BUCKETS_MS: [u64; 12] = [
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000, 300_000, 600_000,
];

#[derive(Default)]
struct DurationHist {
    /// Count per bucket (same index as [`DURATION_BUCKETS_MS`]); values over
    /// the largest bound only land in `+Inf` (i.e. `count`).
    buckets: [u64; DURATION_BUCKETS_MS.len()],
    sum_ms: u64,
    count: u64,
}

#[derive(Default)]
pub struct Metrics {
    // Keyed by label tuple rendered as a stable string, value = counter.
    requests: Mutex<BTreeMap<String, u64>>,
    backend_attempts: Mutex<BTreeMap<String, u64>>,
    fallthroughs: Mutex<BTreeMap<String, u64>>,
    tokens: Mutex<BTreeMap<String, u64>>,
    cost_micros: Mutex<BTreeMap<String, u64>>, // USD * 1e6 to stay integer
    tokens_saved: Mutex<BTreeMap<String, u64>>,
    durations: Mutex<DurationHist>,
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

    /// Records one served request's wall-clock duration.
    pub fn observe_duration(&self, ms: i64) {
        let ms = ms.max(0) as u64;
        let mut h = self.durations.lock().unwrap();
        for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
            if ms <= *bound {
                h.buckets[i] += 1;
            }
        }
        h.sum_ms += ms;
        h.count += 1;
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
        self.render_duration_histogram(&mut out);
        out
    }

    fn render_duration_histogram(&self, out: &mut String) {
        let h = self.durations.lock().unwrap();
        let name = "model_proxy_request_duration_seconds";
        out.push_str(&format!(
            "# HELP {name} Served request duration.\n# TYPE {name} histogram\n"
        ));
        for (i, bound) in DURATION_BUCKETS_MS.iter().enumerate() {
            out.push_str(&format!(
                "{name}_bucket{{le=\"{}\"}} {}\n",
                *bound as f64 / 1000.0,
                h.buckets[i]
            ));
        }
        out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {}\n", h.count));
        out.push_str(&format!("{name}_sum {}\n", h.sum_ms as f64 / 1000.0));
        out.push_str(&format!("{name}_count {}\n", h.count));
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
