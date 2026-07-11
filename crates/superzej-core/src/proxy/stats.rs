//! Pure stats rollups over proxy audit rows (V 298/299): request counts,
//! token/cost totals, latency percentiles, and tokens-per-second throughput,
//! broken down by backend, route, and caller scope.
//!
//! The daemon's `/stats` endpoint, the `superzej proxy stats` CLI, and the TUI
//! dashboard all render this one rollup — the DB supplies raw
//! [`ProxyRequestRow`]s (`proxy_requests_since`) and this module does the math,
//! so the numbers agree everywhere and the logic stays under the core coverage
//! gate.
//!
//! Throughput semantics: a request is **measured** when it recorded a positive
//! `duration_ms` and produced output tokens. Its generation time is
//! `duration_ms - ttfb_ms` (streaming) or `duration_ms` (non-streaming),
//! clamped to ≥1ms. Aggregate tokens/sec is output-token-weighted
//! (`Σ output_tokens / Σ generation seconds`), not a mean of per-request rates,
//! so long requests aren't drowned out by trivial ones.

use serde::Serialize;

use crate::db::ProxyRequestRow;

/// Per-request generation throughput, when the row was measured.
pub fn tokens_per_sec(row: &ProxyRequestRow) -> Option<f64> {
    if row.duration_ms <= 0 || row.output_tokens <= 0 {
        return None;
    }
    let gen_ms = (row.duration_ms - row.ttfb_ms.unwrap_or(0)).max(1);
    Some(row.output_tokens as f64 * 1000.0 / gen_ms as f64)
}

/// Aggregated stats for one grouping key (or the grand total).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Agg {
    pub requests: u64,
    /// Requests whose outcome starts with `ok` (served, incl. streams).
    pub ok: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    /// Nearest-rank percentiles over measured (`duration_ms > 0`) requests.
    pub duration_p50_ms: i64,
    pub duration_p95_ms: i64,
    /// Mean time-to-first-byte over streaming rows (0 when none).
    pub avg_ttfb_ms: i64,
    /// Output-token-weighted generation throughput (0 when unmeasured).
    pub tokens_per_sec: f64,
    /// Throughput of the most recent measured request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tokens_per_sec: Option<f64>,
}

/// An [`Agg`] labelled with its grouping key, for stable JSON output.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NamedAgg {
    pub name: String,
    #[serde(flatten)]
    pub agg: Agg,
}

/// The full rollup: grand totals plus per-backend/route/scope breakdowns
/// (each sorted by request count, descending; name breaks ties).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Rollup {
    pub totals: Agg,
    pub by_backend: Vec<NamedAgg>,
    pub by_route: Vec<NamedAgg>,
    /// Most-specific caller scope per row: `agent:` > `worktree:` >
    /// `workspace:` > `global`.
    pub by_scope: Vec<NamedAgg>,
}

/// Accumulates rows for one grouping key before finalizing into an [`Agg`].
#[derive(Default)]
struct Acc {
    requests: u64,
    ok: u64,
    failed: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    durations: Vec<i64>,
    ttfb_sum: i64,
    ttfb_n: u64,
    gen_ms_sum: i64,
    gen_tokens: u64,
    last_ts: i64,
    last_tps: Option<f64>,
}

impl Acc {
    fn push(&mut self, r: &ProxyRequestRow) {
        self.requests += 1;
        if r.outcome.starts_with("ok") {
            self.ok += 1;
        } else {
            self.failed += 1;
        }
        self.input_tokens += r.input_tokens.max(0) as u64;
        self.output_tokens += r.output_tokens.max(0) as u64;
        self.cost_usd += r.cost_usd;
        if r.duration_ms > 0 {
            self.durations.push(r.duration_ms);
        }
        if let Some(t) = r.ttfb_ms {
            self.ttfb_sum += t.max(0);
            self.ttfb_n += 1;
        }
        if let Some(tps) = tokens_per_sec(r) {
            self.gen_ms_sum += (r.duration_ms - r.ttfb_ms.unwrap_or(0)).max(1);
            self.gen_tokens += r.output_tokens as u64;
            if r.ts_ms >= self.last_ts {
                self.last_ts = r.ts_ms;
                self.last_tps = Some(tps);
            }
        }
    }

    fn finish(mut self) -> Agg {
        self.durations.sort_unstable();
        Agg {
            requests: self.requests,
            ok: self.ok,
            failed: self.failed,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: self.cost_usd,
            duration_p50_ms: percentile(&self.durations, 50),
            duration_p95_ms: percentile(&self.durations, 95),
            avg_ttfb_ms: if self.ttfb_n > 0 {
                self.ttfb_sum / self.ttfb_n as i64
            } else {
                0
            },
            tokens_per_sec: if self.gen_ms_sum > 0 {
                self.gen_tokens as f64 * 1000.0 / self.gen_ms_sum as f64
            } else {
                0.0
            },
            last_tokens_per_sec: self.last_tps,
        }
    }
}

/// Nearest-rank percentile of a **sorted** slice (0 when empty).
fn percentile(sorted: &[i64], p: u32) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p as usize * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// The most-specific caller scope a row was attributed to.
fn scope_of(r: &ProxyRequestRow) -> String {
    if let Some(a) = &r.agent {
        return format!("agent:{a}");
    }
    if let Some(w) = &r.worktree {
        return format!("worktree:{w}");
    }
    if let Some(ws) = &r.workspace {
        return format!("workspace:{ws}");
    }
    "global".to_string()
}

/// Rolls a set of audit rows (any order) into the full stats view.
pub fn rollup(rows: &[ProxyRequestRow]) -> Rollup {
    use std::collections::BTreeMap;
    let mut totals = Acc::default();
    let mut backends: BTreeMap<String, Acc> = BTreeMap::new();
    let mut routes: BTreeMap<String, Acc> = BTreeMap::new();
    let mut scopes: BTreeMap<String, Acc> = BTreeMap::new();
    for r in rows {
        totals.push(r);
        backends.entry(r.backend.clone()).or_default().push(r);
        routes.entry(r.route.clone()).or_default().push(r);
        scopes.entry(scope_of(r)).or_default().push(r);
    }
    let finish = |m: BTreeMap<String, Acc>| -> Vec<NamedAgg> {
        let mut v: Vec<NamedAgg> = m
            .into_iter()
            .map(|(name, acc)| NamedAgg {
                name,
                agg: acc.finish(),
            })
            .collect();
        v.sort_by(|a, b| {
            b.agg
                .requests
                .cmp(&a.agg.requests)
                .then(a.name.cmp(&b.name))
        });
        v
    };
    Rollup {
        totals: totals.finish(),
        by_backend: finish(backends),
        by_route: finish(routes),
        by_scope: finish(scopes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        backend: &str,
        outcome: &str,
        out_tok: i64,
        dur: i64,
        ttfb: Option<i64>,
    ) -> ProxyRequestRow {
        ProxyRequestRow {
            ts_ms: 1000,
            route: "standard".into(),
            backend: backend.into(),
            outcome: outcome.into(),
            input_tokens: 10,
            output_tokens: out_tok,
            cost_usd: 0.01,
            duration_ms: dur,
            ttfb_ms: ttfb,
            ..Default::default()
        }
    }

    #[test]
    fn per_request_tokens_per_sec() {
        // 100 tokens over (2000-500)ms generation → ~66.7 tok/s.
        let r = row("b", "ok_stream", 100, 2000, Some(500));
        let tps = tokens_per_sec(&r).unwrap();
        assert!((tps - 66.666).abs() < 0.01, "{tps}");
        // Non-streaming: full duration is generation time.
        assert_eq!(
            tokens_per_sec(&row("b", "ok", 100, 1000, None)).unwrap(),
            100.0
        );
        // Unmeasured rows yield nothing.
        assert!(tokens_per_sec(&row("b", "ok", 100, 0, None)).is_none());
        assert!(tokens_per_sec(&row("b", "ok", 0, 1000, None)).is_none());
    }

    #[test]
    fn tokens_per_sec_clamps_degenerate_gen_time() {
        // ttfb >= duration → clamp to 1ms, not a division blowup.
        let r = row("b", "ok_stream", 5, 100, Some(100));
        assert_eq!(tokens_per_sec(&r).unwrap(), 5000.0);
    }

    #[test]
    fn rollup_totals_and_outcomes() {
        let rows = vec![
            row("a", "ok", 100, 1000, None),
            row("a", "ok_stream", 50, 1000, Some(200)),
            row("b", "all_failed", 0, 0, None),
        ];
        let r = rollup(&rows);
        assert_eq!(r.totals.requests, 3);
        assert_eq!(r.totals.ok, 2);
        assert_eq!(r.totals.failed, 1);
        assert_eq!(r.totals.input_tokens, 30);
        assert_eq!(r.totals.output_tokens, 150);
        assert!((r.totals.cost_usd - 0.03).abs() < 1e-9);
        // Weighted throughput: 150 tokens over (1000 + 800)ms = ~83.3 tok/s.
        assert!((r.totals.tokens_per_sec - 83.333).abs() < 0.01);
        assert_eq!(r.totals.avg_ttfb_ms, 200);
    }

    #[test]
    fn rollup_groups_and_sorts_by_requests() {
        let rows = vec![
            row("a", "ok", 10, 100, None),
            row("a", "ok", 10, 100, None),
            row("b", "ok", 10, 100, None),
        ];
        let r = rollup(&rows);
        assert_eq!(r.by_backend[0].name, "a");
        assert_eq!(r.by_backend[0].agg.requests, 2);
        assert_eq!(r.by_backend[1].name, "b");
        assert_eq!(r.by_route.len(), 1);
        assert_eq!(r.by_route[0].name, "standard");
    }

    #[test]
    fn scope_prefers_most_specific() {
        let mut r1 = row("a", "ok", 1, 10, None);
        r1.agent = Some("rev".into());
        r1.worktree = Some("/wt".into());
        let mut r2 = row("a", "ok", 1, 10, None);
        r2.worktree = Some("/wt".into());
        let mut r3 = row("a", "ok", 1, 10, None);
        r3.workspace = Some("/repo".into());
        let r4 = row("a", "ok", 1, 10, None);
        let r = rollup(&[r1, r2, r3, r4]);
        let names: Vec<&str> = r.by_scope.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"agent:rev"));
        assert!(names.contains(&"worktree:/wt"));
        assert!(names.contains(&"workspace:/repo"));
        assert!(names.contains(&"global"));
    }

    #[test]
    fn percentiles_nearest_rank() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 95), 4);
        let v: Vec<i64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50), 50);
        assert_eq!(percentile(&v, 95), 95);
    }

    #[test]
    fn last_tokens_per_sec_tracks_newest_measured_row() {
        let mut old = row("a", "ok", 100, 1000, None); // 100 tok/s
        old.ts_ms = 1;
        let mut new = row("a", "ok", 50, 1000, None); // 50 tok/s
        new.ts_ms = 2;
        let unmeasured = ProxyRequestRow {
            ts_ms: 3,
            outcome: "ok".into(),
            ..Default::default()
        };
        let r = rollup(&[old, new, unmeasured]);
        assert_eq!(r.totals.last_tokens_per_sec, Some(50.0));
    }

    #[test]
    fn serializes_to_stable_json() {
        let r = rollup(&[row("a", "ok", 100, 1000, None)]);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["totals"]["requests"], 1);
        assert_eq!(v["by_backend"][0]["name"], "a");
        assert_eq!(v["by_backend"][0]["tokens_per_sec"], 100.0);
    }
}
