# Add Observability Dashboards Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Add a Grafana-compatible observability frontend ("Observe" app-tab) to superzej for metrics/logs/traces with an ad-hoc query REPL and TOML-persisted dashboard layouts.

**Architecture:** A set of new crates (`gtui-core`, `gtui-query`, `gtui-render`, `gtui-app`, `gtui-embed`) that implement a typed columnar `Frame` data model (using Polars), backend-agnostic `DataSource` traits, async off-loop query execution, and terminal rendering (braille fallback via `ratatui` + potential sixel/kitty support). It embeds into superzej via `sz-kit`.

**Tech Stack:** Rust, tokio, ratatui (via `sz-kit`), Polars, reqwest.

---

### Task 1: Create new crates and update Cargo.toml

**Objective:** Scaffold the five new `gtui-*` crates and add them to the superzej workspace.

**Files:**

- Modify: `Cargo.toml`
- Create: `crates/gtui-core/Cargo.toml`
- Create: `crates/gtui-core/src/lib.rs`
- Create: `crates/gtui-query/Cargo.toml`
- Create: `crates/gtui-query/src/lib.rs`
- Create: `crates/gtui-render/Cargo.toml`
- Create: `crates/gtui-render/src/lib.rs`
- Create: `crates/gtui-app/Cargo.toml`
- Create: `crates/gtui-app/src/lib.rs`
- Create: `crates/gtui-embed/Cargo.toml`
- Create: `crates/gtui-embed/src/lib.rs`

**Step 1: Write workspace additions**

```toml
# In Cargo.toml, add to `members`:
  "crates/gtui-core",
  "crates/gtui-query",
  "crates/gtui-render",
  "crates/gtui-app",
  "crates/gtui-embed",
```

**Step 2: Scaffold `gtui-core`**

```toml
# crates/gtui-core/Cargo.toml
[package]
name = "gtui-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
polars = { version = "0.45", features = ["dtype-datetime", "dtype-struct"] }
chrono = { workspace = true }
serde = { workspace = true }
anyhow = { workspace = true }
```

```rust
// crates/gtui-core/src/lib.rs
pub mod frame;
pub mod datasource;
```

**Step 3: Scaffold remaining crates (empty lib.rs with basic dependencies)**
(Create similar `Cargo.toml` files for query, render, app, embed, referencing each other properly, e.g. `gtui-query` depends on `gtui-core`).

**Step 4: Run cargo check**
Run: `cargo check -p gtui-core -p gtui-query -p gtui-render -p gtui-app -p gtui-embed`
Expected: Passes.

**Step 5: Commit**

```bash
git add Cargo.toml crates/gtui-*
git commit -m "feat: scaffold gtui crates for observability dashboards"
```

---

### Task 2: Implement Phase 0.1 - Frame and Field model

**Objective:** Build the `Frame` and `Field` typed columnar model on Polars with time semantics.

**Files:**

- Create: `crates/gtui-core/src/frame.rs`

**Step 1: Write the failing test**

```rust
// crates/gtui-core/src/frame.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_creation() {
        let field = Field::new("value", FieldType::Float64, vec![1.0, 2.0, 3.0]);
        let frame = Frame::new(vec![field]);
        assert_eq!(frame.fields.len(), 1);
    }
}
```

**Step 2: Run test to verify failure**
Run: `cargo test -p gtui-core`
Expected: FAIL - types not defined.

**Step 3: Write minimal implementation**

```rust
// crates/gtui-core/src/frame.rs
use polars::prelude::*;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Time,
    Float64,
    String,
}

pub struct Field {
    pub name: String,
    pub ty: FieldType,
    pub series: Series,
}

impl Field {
    pub fn new(name: &str, ty: FieldType, values: Vec<f64>) -> Self {
        // Simplified for MVP, wrap in Series
        let series = Series::new(name.into(), values);
        Self { name: name.to_string(), ty, series }
    }
}

pub struct Frame {
    pub fields: Vec<Field>,
}

impl Frame {
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }
}
```

**Step 4: Run test to verify pass**
Run: `cargo test -p gtui-core`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/gtui-core/src/frame.rs crates/gtui-core/src/lib.rs
git commit -m "feat: core frame and field model (0.1)"
```

---

### Task 3: Implement Phase 0.2 - DataSource traits

**Objective:** Define the `DataSource` trait, `Query`, `TimeRange`, `Caps`, and `QueryError`.

**Files:**

- Create: `crates/gtui-core/src/datasource.rs`

**Step 1: Write trait definitions**

```rust
// crates/gtui-core/src/datasource.rs
use crate::frame::Frame;
use chrono::{DateTime, Utc};
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;

#[derive(Debug, Clone)]
pub struct TimeRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub ref_id: String,
    pub expr: String,
    pub time_range: TimeRange,
}

#[derive(Debug)]
pub enum QueryError {
    Network(String),
    Auth(String),
    Syntax(String),
    Timeout,
    Other(String),
}

pub trait DataSource: Send + Sync {
    fn query(&self, queries: Vec<Query>) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>>;
}
```

**Step 2: Run cargo check**
Run: `cargo check -p gtui-core`
Expected: PASS

**Step 3: Commit**

```bash
git add crates/gtui-core/src/datasource.rs
git commit -m "feat: datasource traits (0.2)"
```

---

### Task 4: Implement Phase 0.3 - Synthetic Test Source

**Objective:** Implement a random-walk synthetic test source in `gtui-query`.

**Files:**

- Create: `crates/gtui-query/src/synthetic.rs`
- Modify: `crates/gtui-query/src/lib.rs`
- Modify: `crates/gtui-query/Cargo.toml` (ensure it depends on `gtui-core`)

**Step 1: Write failing test**

```rust
// crates/gtui-query/src/synthetic.rs
#[cfg(test)]
mod tests {
    use super::*;
    use gtui_core::datasource::{Query, TimeRange};
    use chrono::Utc;

    #[tokio::test]
    async fn test_synthetic_query() {
        let source = SyntheticSource::new();
        let q = Query {
            ref_id: "A".to_string(),
            expr: "random_walk".to_string(),
            time_range: TimeRange { from: Utc::now(), to: Utc::now() }
        };
        let res = source.query(vec![q]).await.unwrap();
        assert_eq!(res.len(), 1);
    }
}
```

**Step 2: Add dependencies and test**
Add `tokio` (with macros/rt) to `gtui-query` dev-dependencies.
Run: `cargo test -p gtui-query`
Expected: FAIL - missing implementation.

**Step 3: Write implementation**

```rust
// crates/gtui-query/src/synthetic.rs
use gtui_core::datasource::{DataSource, Query, QueryError, TimeRange};
use gtui_core::frame::{Frame, Field, FieldType};
use std::future::Future;
use std::pin::Pin;

pub struct SyntheticSource;

impl SyntheticSource {
    pub fn new() -> Self { Self }
}

impl DataSource for SyntheticSource {
    fn query(&self, queries: Vec<Query>) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
        let res = queries.into_iter().map(|q| {
            let field = Field::new("val", FieldType::Float64, vec![1.0, 1.5, 2.0]); // Mocked walk
            Frame::new(vec![field])
        }).collect();
        Box::pin(async move { Ok(res) })
    }
}
```

**Step 4: Run test to verify pass**
Run: `cargo test -p gtui-query`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/gtui-query
git commit -m "feat: synthetic test data source (0.3)"
```

---

### Task 5: Implement Phase 0.4 - Braille Time-Series rendering

**Objective:** Implement braille time-series rendering with downsampling in `gtui-render`.

**Files:**

- Create: `crates/gtui-render/src/timeseries.rs`
- Modify: `crates/gtui-render/src/lib.rs`
- Modify: `crates/gtui-render/Cargo.toml` (depend on `gtui-core`, `sz-kit`, `ratatui`)

**Step 1: Write test for downsample/render**
(Write a unit test that verifies the output bounds of a downsampled series)

**Step 2: Implement braille time-series**
(Use `ratatui::widgets::canvas` or a custom braille plotter. Implement LTTB min-max downsampling on the `Frame` data).

**Step 3: Test and Commit**
Run: `cargo test -p gtui-render`

```bash
git commit -m "feat: braille time-series rendering (0.4)"
```

---

### Task 6: Implement Phase 0.5 - Event loop and App Shell

**Objective:** Create the `gtui-app` event loop, global time range, and off-loop render integration.

**Files:**

- Create: `crates/gtui-app/src/app.rs`
- Modify: `crates/gtui-app/src/lib.rs`

**Step 1: Implement App state**
Define `ObserveApp` with time range, layout state, and channel receivers for async queries.

**Step 2: Implement update loop**
Implement the logic to drain channels, pulse the waker (via a passed `std::sync::Arc<dyn Fn()>`), and trigger renders.

**Step 3: Test and Commit**
Verify no polling timeouts exist in the loop logic.

```bash
git commit -m "feat: observe app shell and event loop (0.5)"
```

---

_(The plan continues with Phase 1 through 3 tasks following the same structured approach, adhering to the OpenSpec design documents.)_

### Handoff

Plan complete and saved. Ready to execute using subagent-driven-development — I'll dispatch a fresh subagent per task with two-stage review (spec compliance then code quality). Shall I proceed?
