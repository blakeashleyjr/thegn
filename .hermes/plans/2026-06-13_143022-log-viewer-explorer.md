# Log Viewer / Explorer Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Build a highly performant log viewer/explorer as a section in the right panel (or a dedicated modal) capable of ingesting `thegn` logs and external application/container logs (via Docker/Podman/Systemd) with live tailing, filtering, and export.

**Architecture:**

1. **Model:** Extend `PanelData` with a `Logs` section or add a new `LogsData` structure populated via a background hydration thread that tails specific log streams.
2. **Ingestion Layer:** Introduce a new abstraction `LogSource` (e.g., `SzhostLog`, `ContainerLog`, `FileLog`) in the `thegn-svc` or `thegn-core` layer to handle tailing and rotation. The loop communicates with these sources via `mpsc` channels, pulsing the waker on new data.
3. **Storage:** Keep the most recent N lines (e.g., 10k) in a `VecDeque` or `HistoryBuffer` (similar to `PtyPane`). Do not store them in the SQLite DB to avoid WAL write-amplification for high-throughput logs.
4. **UI:** Add a `Logs` variant to `PanelHit` and `Section`. The UI renders `PanelRow`s from the buffer. Features include filtering (via a sidebar-like filter input), auto-scrolling, and an action to export to a file. Auto-import from containers integrates with `thegn_core::sandbox::running_containers()`.

**Tech Stack:** Rust, `tokio`, `termwiz`, `mpsc` channels.

---

### Task 1: Define the Log Data Model and `Section::Logs`

**Objective:** Add the `Logs` section to the panel and define the core data structures for log lines.

**Files:**

- Modify: `crates/thegn-host/src/panel/mod.rs`
- Create: `crates/thegn-host/src/panel/logs.rs`

**Step 1: Write failing test**
(Skipped for config changes, but we'll add a test for the new section enum parsing)

```rust
// tests/panel_section.rs or similar
#[test]
fn test_section_logs_parses() {
    assert_eq!(Section::from_key("logs"), Some(Section::Logs));
}
```

**Step 2: Write minimal implementation**

1. Add `Logs` to `crates/thegn-host/src/panel/mod.rs` `Section` enum and update `SECTION_ORDER`, `label()`, `is_git_family()`, `home_view()`.
2. Create `crates/thegn-host/src/panel/logs.rs` with:

   ```rust
   use std::collections::VecDeque;

   #[derive(Debug, Clone, PartialEq, Eq)]
   pub struct LogLine {
       pub timestamp: i64,
       pub level: String,
       pub source: String,
       pub content: String,
   }

   #[derive(Debug, Clone, Default)]
   pub struct LogsPanelState {
       pub lines: VecDeque<LogLine>,
       pub filter: String,
       pub active_sources: Vec<String>,
       pub follow: bool, // auto-tail
   }
   ```

3. Add `logs: LogsPanelState` to `PanelUi`.

**Step 3: Commit**

```bash
git add crates/thegn-host/src/panel/mod.rs crates/thegn-host/src/panel/logs.rs
git commit -m "feat(panel): Add Logs section and basic data structures"
```

---

### Task 2: Implement the Log Ingestion Worker (thegn logs)

**Objective:** Create a background worker that tails `$XDG_STATE_HOME/thegn/logs/thegn.log` and feeds it to the event loop.

**Files:**

- Modify: `crates/thegn-host/src/hydrate.rs`
- Modify: `crates/thegn-host/src/run.rs`

**Step 1: Implement the Worker**

In `hydrate.rs`:

```rust
pub(crate) fn spawn_log_tailer(
    log_file: std::path::PathBuf,
    tx: tokio_mpsc::UnboundedSender<Vec<crate::panel::logs::LogLine>>,
    waker: termwiz::terminal::TerminalWaker,
) {
    std::thread::spawn(move || {
        // Simple polling tailer (inotify could also be used, but polling is robust for logs)
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(&log_file) else { return };
        // Start at EOF for now, or read last N bytes. Let's read last 4KB to seed.
        let _ = file.seek(SeekFrom::End(-4096));
        let mut buf = String::new();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            buf.clear();
            if file.read_to_string(&mut buf).is_ok() && !buf.is_empty() {
                let lines: Vec<_> = buf.lines().map(|l| crate::panel::logs::LogLine {
                    timestamp: crate::run::now_secs(),
                    level: "INFO".into(), // basic parse, refine later
                    source: "thegn".into(),
                    content: l.to_string(),
                }).collect();
                if tx.send(lines).is_err() { break; }
                let _ = waker.wake();
            }
        }
    });
}
```

**Step 2: Wire it into the Event Loop**

In `run.rs`:

1. Add `let (logs_tx, mut logs_rx) = tokio_mpsc::unbounded_channel();`
2. Spawn the tailer pointing to `thegn_core::util::thegn_dir().join("logs/thegn.log")`.
3. In the `poll_input` loop, drain `logs_rx` and append to `panel_ui.logs.lines` (cap at e.g., 5000), then set `dirty = true`.

**Step 3: Commit**

```bash
git add crates/thegn-host/src/hydrate.rs crates/thegn-host/src/run.rs
git commit -m "feat(logs): Add background tailer for thegn logs"
```

---

### Task 3: Render the Logs Section

**Objective:** Implement `sections::misc::logs` to render `LogsPanelState` into `PanelRow`s.

**Files:**

- Modify: `crates/thegn-host/src/panel/sections/misc.rs`
- Modify: `crates/thegn-host/src/panel/sections/mod.rs` (to route it)

**Step 1: Implementation**

In `misc.rs`:

```rust
pub(super) fn logs(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (ui, deep, full) = (ctx.ui, ctx.deep(), ctx.full());
    let mut rows = Vec::new();

    // Header controls
    rows.push(PanelRow::plain(Line::split(
        vec![seg(g2(), "SOURCES"), seg(d(), ui.logs.active_sources.join(", "))],
        vec![seg(if ui.logs.follow { hue(Hue::Green) } else { g2() }, "f follow")],
    )));
    rows.push(PanelRow::blank());

    // Filter
    if !ui.logs.filter.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(hue(Hue::Amber), format!("Filter: {}", ui.logs.filter))])));
    }

    // Lines (filtered)
    let visible_lines: Vec<_> = ui.logs.lines.iter()
        .filter(|l| ui.logs.filter.is_empty() || l.content.contains(&ui.logs.filter))
        .collect();

    // Render N lines based on budget
    for (i, line) in visible_lines.iter().enumerate().rev().take(ctx.rows).rev() {
        rows.push(PanelRow::plain(Line::segs(vec![
            seg(g(), &line.source),
            sp(1),
            seg(d(), &line.content),
        ])));
    }

    rows
}
```

**Step 2: Commit**

```bash
git add crates/thegn-host/src/panel/sections/misc.rs crates/thegn-host/src/panel/sections/mod.rs
git commit -m "feat(panel): Render the logs accordion section"
```

---

### Task 4: Auto-import Container Logs

**Objective:** Detect running containers related to the worktree and spawn tailers for `podman logs -f <container>`.

**Files:**

- Modify: `crates/thegn-host/src/hydrate.rs`
- Modify: `crates/thegn-host/src/run.rs`

**Step 1: Implement Container Log Tailer**

In `hydrate.rs`:

```rust
pub(crate) fn spawn_container_log_tailer(
    backend: &str,
    container_name: String,
    tx: tokio_mpsc::UnboundedSender<Vec<crate::panel::logs::LogLine>>,
    waker: termwiz::terminal::TerminalWaker,
) {
    let backend = backend.to_string();
    std::thread::spawn(move || {
        let mut child = std::process::Command::new(backend)
            .args(&["logs", "-f", "--tail", "100", &container_name])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()) // docker/podman multiplexes stderr
            .spawn()
            .unwrap();

        let out = child.stdout.take().unwrap();
        let reader = std::io::BufReader::new(out);
        use std::io::BufRead;
        for line in reader.lines() {
            if let Ok(l) = line {
                 let line_obj = crate::panel::logs::LogLine {
                    timestamp: crate::run::now_secs(),
                    level: "INFO".into(),
                    source: container_name.clone(),
                    content: l,
                };
                if tx.send(vec![line_obj]).is_err() { break; }
                let _ = waker.wake();
            }
        }
        let _ = child.kill();
    });
}
```

**Step 2: Wire Auto-Discovery**

In `run.rs`, when the active container (`model.active_container_name`) changes or on launch, if `model.containers` has it running, trigger `spawn_container_log_tailer`. You'll need to keep track of active tailers (e.g., in a `HashMap` of active processes or just track by name so you don't spawn duplicates).

**Step 3: Commit**

```bash
git add crates/thegn-host/src/hydrate.rs crates/thegn-host/src/run.rs
git commit -m "feat(logs): Auto-tail worktree container logs"
```

---

### Task 5: Export Functionality

**Objective:** Allow exporting the current log buffer to a file.

**Files:**

- Modify: `crates/thegn-host/src/panel/mod.rs` (intent)
- Modify: `crates/thegn-host/src/run.rs` (action)

**Step 1: Implementation**

1. Add `PanelMsg::ExportLogs` or handle a specific keybind (e.g., `E` while logs section is open).
2. In `run.rs`, handle the action:

```rust
// Ask user for path (or auto-generate in XDG_STATE_HOME/thegn/exports/)
let export_path = thegn_core::util::thegn_dir().join("exports").join(format!("logs_{}.txt", now_secs()));
let lines: Vec<String> = panel_ui.logs.lines.iter().map(|l| format!("[{}] {} {}", l.source, l.level, l.content)).collect();
std::fs::create_dir_all(export_path.parent().unwrap());
std::fs::write(&export_path, lines.join("\n"));
model.status = format!("Logs exported to {:?}", export_path);
```

**Step 2: Commit**

```bash
git commit -am "feat(logs): Add log export functionality"
```

---

### Risks and Tradeoffs

1. **Performance:** `podman logs -f` blocks a thread per container. If many worktrees are open, this could scale poorly. _Mitigation:_ Only spawn the tailer for the _currently focused_ worktree container. Kill the child process when switching tabs/worktrees.
2. **Memory:** Keeping 10,000 strings in a `VecDeque` on the UI thread takes memory. _Mitigation:_ Cap it strictly (e.g., 2000 lines). The underlying log file or container runtime retains the full history if needed.
3. **ANSI Parsing:** Container logs often include ANSI color codes. _Mitigation:_ Use `termwiz::escape::parser::Parser` or the existing `AnsiStripper` used by `PtyPane` to strip codes before storing `content`, OR store raw strings and use `termwiz` to render them safely.
4. **Multiplexed Streams:** Docker multiplexes stdout/stderr into the same stream with headers if TTY is false. _Mitigation:_ Ensure we read correctly or force TTY via `podman logs -t`.

**Next Steps:** Review plan and dispatch via `subagent-driven-development`.
