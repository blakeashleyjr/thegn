# Prometheus Metrics Sidebar Integration Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Add a Metrics section to the sidebar that scrapes Prometheus `/metrics` endpoints directly and displays allowlisted metrics with health status indicators — zero external Prometheus server dependency.

**Architecture:**

1. **Config layer** — Add `[metrics]` table to `crates/thegn-core/src/config.rs` following the existing `StatsConfig` pattern with `targets`, `interval`, `timeout`, and `max_body_bytes` fields.
2. **Core scraper** — Add a minimal Prometheus text-format parser in `thegn-core` that extracts gauge/counter/untyped samples and supports label matching for allowlisted metrics.
3. **Host supervisor** — Add a `MetricsSupervisor` in `thegn-host` that runs off-thread (like `StatsSampler`), scrapes targets on a configurable interval, caches latest values + error states, and sends updates over an mpsc channel + pulses the TerminalWaker.
4. **Sidebar rendering** — Extend `FrameModel` with `metrics` data and add a `draw_metrics_section` function that renders the METRICS section below WORKSPACES (or as a collapsible section), with per-target health indicators (● up, stale, err) and allowlisted metric values.
5. **TUI integration** — Wire the metrics channel into the event loop alongside `stats_rx`, with damage-tracked redraws.

**Tech Stack:** Rust, tokio, serde (already in workspace), reqwest (add as dep), native HTTP scraping.

---

### Task 1: Add `[metrics]` Config to thegn-core

**Objective:** Define the configuration structure for Prometheus scrape targets.

**Files:**

- Modify: `crates/thegn-core/src/config.rs`
- Test: `crates/thegn-core/src/config.rs` (add unit test for defaults)

**Step 1: Write the config structs**

Add after `StatsConfig` (around line 537):

```rust
/// `[metrics]` — Prometheus scrape targets for sidebar metrics display.
/// Each target is scraped directly via HTTP; no Prometheus server required.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MetricsConfig {
    /// Scrape interval in seconds.
    pub interval_secs: f64,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Max response body size in bytes (prevent runaway).
    pub max_body_bytes: usize,
    /// Scrape targets.
    pub targets: Vec<MetricsTarget>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        MetricsConfig {
            interval_secs: 5.0,
            timeout_ms: 500,
            max_body_bytes: 1_048_576, // 1 MiB
            targets: Vec::new(),
        }
    }
}

/// One Prometheus scrape target.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MetricsTarget {
    /// Display name in the sidebar.
    pub name: String,
    /// URL to scrape (e.g., `http://localhost:9091/metrics`).
    pub url: String,
    /// Metrics to display (allowlist). Empty = all.
    #[serde(default)]
    pub metrics: Vec<String>,
    /// Optional labels to match (e.g., `instance="localhost:9091"`).
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
}
```

**Step 2: Add `metrics` field to `Config` struct**

In `Config` (around line 1120), add:

```rust
pub metrics: MetricsConfig,
```

And in `Config::default()`:

```rust
metrics: MetricsConfig::default(),
```

**Step 3: Add to `ConfigOverlay`**

Add to `ConfigOverlay` (around line 1252):

```rust
pub metrics: Option<MetricsTarget>,
```

Wait — `MetricsTarget` is a Vec, so we need a different pattern. Add a `MetricsOverlay`:

```rust
#[derive(Debug, Default)]
pub struct MetricsOverlay {
    pub interval_secs: Option<f64>,
    pub timeout_ms: Option<u64>,
    pub max_body_bytes: Option<usize>,
    pub targets: Vec<MetricsTarget>,
}

impl MetricsOverlay {
    fn apply(self, base: &mut MetricsConfig) {
        if let Some(v) = self.interval_secs {
            base.interval_secs = v;
        }
        if let Some(v) = self.timeout_ms {
            base.timeout_ms = v;
        }
        if let Some(v) = self.max_body_bytes {
            base.max_body_bytes = v;
        }
        if !self.targets.is_empty() {
            base.targets = self.targets;
        }
    }
}
```

Then in `ConfigOverlay`:

```rust
pub metrics: MetricsOverlay,
```

And in `ConfigOverlay::apply`:

```rust
if !self.metrics.targets.is_empty() || /* other non-empty checks */ {
    self.metrics.apply(&mut base.metrics);
}
```

**Step 4: Run test to verify pass**

```bash
cd /home/blake/code/thegn
cargo test -p thegn-core config
```

**Step 5: Commit**

```bash
git add crates/thegn-core/src/config.rs
git commit -m "feat(config): add [metrics] section for prometheus scrape targets"
```

---

### Task 2: Prometheus Text-Format Parser in thegn-core

**Objective:** Add a minimal parser that extracts gauge/counter/untyped samples from Prometheus text format.

**Files:**

- Create: `crates/thegn-core/src/metrics.rs`
- Test: `crates/thegn-core/src/metrics.rs` (inline tests)

**Step 1: Write the parser**

```rust
//! Minimal Prometheus text-format parser.
//! Only extracts gauge/counter/untyped samples; ignores histograms/summaries.

use std::collections::HashMap;

/// One metric sample.
#[derive(Debug, Clone)]
pub struct MetricSample {
    pub name: String,
    pub value: f64,
    pub labels: HashMap<String, String>,
}

/// Parse Prometheus text format into samples.
pub fn parse_metrics(input: &str) -> Vec<MetricSample> {
    let mut samples = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        // Skip comments and blank lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Parse: metric_name{label="value",...} number
        if let Some((name_and_labels, value_str)) = line.split_once(' ') {
            let value: f64 = match value_str.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Extract labels and name
            let (name, labels) = if let Some((name, labels)) = name_and_labels.trim_start_matches('{').split_once('}') {
                let mut labels_map = HashMap::new();
                for label_pair in labels.split(',') {
                    if let Some((k, v)) = label_pair.split_once('=') {
                        let v = v.trim_matches('"');
                        labels_map.insert(k.trim().to_string(), v.to_string());
                    }
                }
                (name.to_string(), labels_map)
            } else {
                (name_and_labels.to_string(), HashMap::new())
            };
            samples.push(MetricSample { name, value, labels });
        }
    }
    samples
}

/// Filter samples by allowlisted metric names and optional label matchers.
pub fn filter_samples(
    samples: &[MetricSample],
    allowlist: &[String],
    labels: &HashMap<String, String>,
) -> Vec<MetricSample> {
    samples
        .iter()
        .filter(|s| {
            // Name filter
            if !allowlist.is_empty() && !allowlist.iter().any(|p| s.name.starts_with(p)) {
                return false;
            }
            // Label filter
            for (k, v) in labels {
                match s.labels.get(k) {
                    Some(actual) if actual == v => {}
                    _ => return false,
                }
            }
            true
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_gauge() {
        let input = r#"
# HELP http_requests_total Total requests
# TYPE http_requests_total counter
http_requests_total{method="GET"} 12345
process_resident_memory_bytes 82440192
"#;
        let samples = parse_metrics(input);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].name, "http_requests_total");
        assert_eq!(samples[0].value, 12345.0);
        assert_eq!(samples[0].labels.get("method"), Some(&"GET".to_string()));
        assert_eq!(samples[1].name, "process_resident_memory_bytes");
        assert_eq!(samples[1].value, 82440192.0);
    }

    #[test]
    fn test_filter_by_name() {
        let samples = vec![
            MetricSample { name: "http_requests_total".into(), value: 10.0, labels: HashMap::new() },
            MetricSample { name: "go_goroutines".into(), value: 42.0, labels: HashMap::new() },
        ];
        let filtered = filter_samples(&samples, &["http_".into()], &HashMap::new());
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "http_requests_total");
    }
}
```

**Step 2: Run tests**

```bash
cargo test -p thegn-core metrics
```

**Step 3: Commit**

```bash
git add crates/thegn-core/src/metrics.rs
git commit -m "feat(core): add minimal prometheus text-format parser"
```

---

### Task 3: MetricsSupervisor in thegn-host

**Objective:** Create an off-thread supervisor that scrapes targets on interval and sends updates to the TUI.

**Files:**

- Create: `crates/thegn-host/src/metrics.rs`
- Modify: `crates/thegn-host/src/main.rs` (add `mod metrics;`)
- Modify: `crates/thegn-host/src/run.rs` (wire channel)

**Step 1: Write the supervisor**

```rust
//! Metrics scraper supervisor — runs off-thread, scrapes Prometheus endpoints,
//! and sends updates to the TUI via mpsc channel.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::sleep;

use thegn_core::config::MetricsConfig;
use thegn_core::metrics::{filter_samples, parse_metrics};

/// One target's latest state.
#[derive(Debug, Clone)]
pub struct MetricTargetState {
    pub name: String,
    pub url: String,
    /// Latest samples (filtered to allowlist).
    pub samples: Vec<thegn_core::metrics::MetricSample>,
    /// Health state.
    pub health: MetricHealth,
    /// Last successful scrape timestamp (for stale detection).
    pub last_ok: Option<Instant>,
    /// Error message if unhealthy.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetricHealth {
    Up,
    Stale,
    Error,
}

/// All targets' latest state.
#[derive(Debug, Clone, Default)]
pub struct MetricsState {
    pub targets: Vec<MetricTargetState>,
}

/// Spawn the metrics supervisor. Runs on a background tokio task.
pub fn spawn_metrics_supervisor(
    config: MetricsConfig,
    tx: mpsc::UnboundedSender<MetricsState>,
    waker: termwiz::terminal::TerminalWaker,
) {
    if config.targets.is_empty() {
        return;
    }

    tokio::spawn(async move {
        let interval = Duration::from_secs_f64(config.interval_secs.max(1.0));
        let timeout = Duration::from_millis(config.timeout_ms.max(100).min(30000));

        // Initialize state with all targets
        let mut state = MetricsState {
            targets: config.targets.iter().map(|t| MetricTargetState {
                name: t.name.clone(),
                url: t.url.clone(),
                samples: Vec::new(),
                health: MetricHealth::Error,
                last_ok: None,
                error: Some("initializing".into()),
            }).collect(),
        };

        // Send initial state
        let _ = tx.send(state.clone());

        loop {
            let now = Instant::now();

            for (i, target_cfg) in config.targets.iter().enumerate() {
                let result = scrape_target(&target_cfg.url, timeout, config.max_body_bytes).await;

                let target_state = &mut state.targets[i];
                target_state.samples = match result {
                    Ok(body) => {
                        let all_samples = parse_metrics(&body);
                        let filtered = filter_samples(
                            &all_samples,
                            &target_cfg.metrics,
                            &target_cfg.labels,
                        );
                        target_state.health = MetricHealth::Up;
                        target_state.last_ok = Some(now);
                        target_state.error = None;
                        filtered
                    }
                    Err(e) => {
                        // Check staleness
                        if let Some(last_ok) = target_state.last_ok {
                            if now.duration_since(last_ok) > interval * 2 + timeout {
                                target_state.health = MetricHealth::Stale;
                            }
                        } else {
                            target_state.health = MetricHealth::Error;
                        }
                        target_state.error = Some(e.clone());
                        Vec::new()
                    }
                };
            }

            // Send update
            let _ = tx.send(state.clone());
            let _ = waker.wake();

            sleep(interval).await;
        }
    });
}

/// Scrape a single target.
async fn scrape_target(url: &str, timeout: Duration, max_bytes: usize) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;

    let response = client
        .get(url)
        .header("Accept", "text/plain; version=0.0.4")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let bytes = response.bytes().await.map_err(|e| e.to_string())?;

    if bytes.len() > max_bytes {
        return Err(format!("response too large: {} > {}", bytes.len(), max_bytes));
    }

    String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
}
```

**Step 2: Add `mod metrics;` to main.rs**

In `crates/thegn-host/src/main.rs`, add:

```rust
mod metrics;
```

**Step 3: Wire into run.rs**

In the `main` function (around where `spawn_refresh_ticker` is called), add:

```rust
// Metrics scraper supervisor
let (metrics_tx, metrics_rx) = tokio_mpsc::unbounded_channel::<crate::metrics::MetricsState>();
crate::metrics::spawn_metrics_supervisor(
    cfg.metrics.clone(),
    metrics_tx,
    waker.clone(),
);

// Pass metrics_rx to event_loop
event_loop(
    // ... existing args ...
    metrics_rx,
    // ...
).await?;
```

**Step 4: Update event_loop signature**

In `event_loop` function signature (around line 2310), add:

```rust
mut metrics_rx: tokio_mpsc::UnboundedReceiver<crate::metrics::MetricsState>,
```

**Step 5: Add reqwest to Cargo.toml**

In `crates/thegn-host/Cargo.toml`, add:

```rust
reqwest = { version = "0.12", features = ["json"] }
```

**Step 6: Build and verify**

```bash
cargo build -p thegn-host
```

**Step 7: Commit**

```bash
git add crates/thegn-host/src/metrics.rs crates/thegn-host/src/main.rs crates/thegn-host/src/run.rs crates/thegn-host/Cargo.toml
git commit -m "feat(host): add metrics scraper supervisor for prometheus endpoints"
```

---

### Task 4: Wire MetricsState into FrameModel

**Objective:** Add metrics data to the chrome model so the renderer can access it.

**Files:**

- Modify: `crates/thegn-host/src/chrome.rs` (add `metrics` to `FrameModel`)
- Modify: `crates/thegn-host/src/hydrate.rs` (sample metrics into model)

**Step 1: Add to FrameModel**

In `FrameModel` struct (around line 315), add:

```rust
/// Latest metrics state for the sidebar section.
pub metrics: crate::metrics::MetricsState,
```

**Step 2: Handle in event loop**

In `run.rs`, inside the event loop (around where `stats_rx` is handled), add:

```rust
// Fresh metrics reading from the scraper supervisor.
while let Ok(state) = metrics_rx.try_recv() {
    if model.metrics != state {
        model.metrics = state;
        dirty = true;
    }
}
```

Wait — need to derive `PartialEq` for `MetricsState`. Add to `metrics.rs`:

```rust
impl PartialEq for MetricsState {
    fn eq(&self, other: &Self) -> bool {
        self.targets.len() == other.targets.len()
            && self.targets.iter().zip(other.targets.iter()).all(|(a, b)| {
                a.name == b.name
                    && a.health == b.health
                    && a.error == b.error
                    && a.samples.len() == b.samples.len()
            })
    }
}
```

**Step 3: Build and verify**

```bash
cargo build -p thegn-host
```

**Step 4: Commit**

```bash
git add crates/thegn-host/src/chrome.rs crates/thegn-host/src/run.rs crates/thegn-host/src/metrics.rs
git commit -m "feat(host): wire metrics state into frame model"
```

---

### Task 5: Render METRICS Section in Sidebar

**Objective:** Draw a METRICS section below WORKSPACES in the sidebar with target health and metric values.

**Files:**

- Modify: `crates/thegn-host/src/chrome.rs` (add `draw_metrics_section`)

**Step 1: Add render function**

Add after `draw_sandbox_section` (around line 1356):

```rust
/// The METRICS section: per-target health + allowlisted metric values.
fn draw_metrics_section(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    if rect.rows < 2 {
        return;
    }
    // Section rule + title.
    let line = "\u{2500}".repeat(rect.cols);
    draw_text(
        surface,
        rect.x,
        rect.y,
        &line,
        col(S::Border),
        col(S::Panel),
        rect.cols,
    );
    draw_text_bold(
        surface,
        rect.x + 1,
        rect.y,
        " METRICS ",
        col(S::Text),
        col(S::Panel),
        rect.cols.saturating_sub(1),
    );

    if model.metrics.targets.is_empty() {
        draw_text(
            surface,
            rect.x + 1,
            rect.y + 1,
            "none configured",
            col(S::Faint),
            col(S::Panel),
            rect.cols.saturating_sub(1),
        );
        return;
    }

    let mut y = rect.y + 1;
    let max_y = rect.y + rect.rows;

    for target in &model.metrics.targets {
        if y >= max_y {
            break;
        }
        // Health indicator
        let (dot, dot_fg, health_str) = match target.health {
            crate::metrics::MetricHealth::Up => ("\u{25cf}", theme_color(theme::GREEN), "up"),
            crate::metrics::MetricHealth::Stale => ("\u{25cb}", col(S::Dim), "stale"),
            crate::metrics::MetricHealth::Error => ("\u{25cb}", theme_color(theme::RED), "err"),
        };
        draw_text(surface, rect.x + 1, y, dot, dot_fg, col(S::Panel), 1);

        // Target name
        draw_text(
            surface,
            rect.x + 3,
            y,
            &target.name,
            col(S::Text),
            col(S::Panel),
            rect.cols.saturating_sub(3),
        );

        // Health status
        let health_col = rect.x + 3 + target.name.chars().count() + 2;
        if health_col < rect.x + rect.cols {
            draw_text(
                surface,
                health_col,
                y,
                health_str,
                dot_fg,
                col(S::Panel),
                (rect.x + rect.cols).saturating_sub(health_col),
            );
        }

        y += 1;

        // Show first few metric values (if up)
        if target.health == crate::metrics::MetricHealth::Up {
            for sample in target.samples.iter().take(3) {
                if y >= max_y {
                    break;
                }
                let line = format!("  {} {}", sample.name, sample.value as u64);
                draw_text(
                    surface,
                    rect.x + 1,
                    y,
                    &line,
                    col(S::Dim),
                    col(S::Panel),
                    rect.cols.saturating_sub(1),
                );
                y += 1;
            }
        } else if let Some(ref err) = target.error {
            if y < max_y {
                let err_msg = format!("  err: {}", err);
                draw_text(
                    surface,
                    rect.x + 1,
                    y,
                    &err_msg,
                    col(S::Faint),
                    col(S::Panel),
                    rect.cols.saturating_sub(1),
                );
                y += 1;
            }
        }
    }
}
```

**Step 2: Integrate into draw_sidebar**

In `draw_sidebar` (around line 1092, after the row loop), add:

```rust
// Draw METRICS section if there's room
if rect.rows > 10 {
    let metrics_rect = Rect {
        x: rect.x,
        y: rect.y + rect.rows - 6,
        cols: rect.cols,
        rows: 6,
    };
    draw_metrics_section(surface, metrics_rect, model);
}
```

**Step 3: Build and verify**

```bash
cargo build -p thegn-host
```

**Step 4: Commit**

```bash
git add crates/thegn-host/src/chrome.rs
git commit -m "feat(chrome): render metrics section in sidebar"
```

---

### Task 6: Example Config and Documentation

**Objective:** Document the feature in config.toml.example and add example usage.

**Files:**

- Modify: `config/config.toml.example`

**Step 1: Add to config.toml.example**

Add after the `[stats]` section (around line 100):

```toml
# Prometheus scrape targets for the sidebar METRICS section.
# Each target is scraped directly via HTTP; no Prometheus server required.
# The sidebar shows target health (● up, stale, err) and allowlisted metric values.
[metrics]
interval-secs = 5           # Scrape interval in seconds
timeout-ms = 500             # Request timeout in milliseconds
max-body-bytes = 1048576   # Max response size (1 MiB)

# Example: monitor a local model-proxy service
# [[metrics.targets]]
# name = "model-proxy"
# url = "http://127.0.0.1:9091/metrics"
# metrics = ["http_requests_total", "process_resident_memory_bytes", "model_proxy_active_requests"]
```

**Step 2: Commit**

```bash
git add config/config.toml.example
git commit -m "docs(config): add [metrics] section example"
```

---

## Verification Steps

1. **Config loads:**

   ```bash
   cargo run -p thegn-host --bin thegn --config /dev/null
   # Should not panic on missing [metrics] section
   ```

2. **Parser tests:**

   ```bash
   cargo test -p thegn-core metrics
   ```

3. **Full build:**

   ```bash
   cargo build --workspace
   ```

4. **E2E test:**
   - Add a `[[metrics.targets]]` to config with a real endpoint
   - Launch thegn
   - Verify METRICS section appears in sidebar with health indicator

---

## Risks & Tradeoffs

- **No history/rate calculation:** Direct scrape doesn't store history, so no rate graphs. Could add simple rate on top later if needed.
- **HTTP dependency:** Adds `reqwest` to host. Alternative: use `ureq` (sync, no tokio) or native HTTP. Chose reqwest for async simplicity.
- **Stale detection:** Uses `2 * interval + timeout` as stale threshold. May need tuning.
- **No PromQL:** Not supported by design — would require full Prometheus server. Could add optional `/api/v1/query` target type later.

---

## Open Questions

1. Should metrics section be collapsible like WORKSPACES?
2. Should we support basic rate calculation (delta between scrapes)?
3. Should we add a keyboard shortcut to refresh metrics manually?
4. Should the section be below WORKSPACES or in a separate region (like the panel's SANDBOXES)?

These can be addressed as follow-up refinements after the MVP ships.
