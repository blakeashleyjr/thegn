# Dashboard Integration Implementation Plan

> **For Hermes:** Use `subagent-driven-development` skill to implement this plan task-by-task.

**Goal:** Create a terminal-native metrics and status dashboard tab integrated into thegn, with identical UX to existing `chat` and `comms` embedded applications. The dashboard will utilize the existing `[metrics]` and `[dashboard]` configuration blocks, providing an overview similar to `WTF` but built on the thegn/ratatui foundation.

**Architecture:**

1. Add a new `dashboard` embedded app (`crates/thegn-host/src/apps/dashboard.rs`) that satisfies the `tg_kit::AppTile` contract.
2. The UI structure will use standard `ratatui` primitives, matching the existing `tg_kit::Theme` integration.
3. Hook up the existing `Action::Dashboard` keybinding to open the dashboard app tab.

**Tech Stack:**

- Rust (existing `thegn-host`, `tg-kit`)
- `ratatui` for TUI components
- `reqwest` / `serde` (or existing metrics components) for data fetching

---

### Task 1: Create the Dashboard App Tile Module

**Objective:** Add `crates/thegn-host/src/apps/dashboard.rs` implementing `AppTile` and configure it as an app slot.

**Files:**

- Create: `crates/thegn-host/src/apps/dashboard.rs`
- Modify: `crates/thegn-host/src/apps/mod.rs`
- Modify: `crates/thegn-host/src/run.rs`

**Step 1: Write the minimal module and tile structure**

```rust
// crates/thegn-host/src/apps/dashboard.rs
use tg_kit::ratatui::buffer::Buffer;
use tg_kit::ratatui::layout::Rect;
use tg_kit::ratatui::widgets::{Block, Borders, Paragraph};
use tg_kit::{AppTile, ChangeHook, InputEvent, InputResult, Theme};

pub struct DashboardUi {
    theme: Theme,
}

impl DashboardUi {
    pub fn new(_on_change: Option<ChangeHook>, theme: Theme) -> Self {
        Self { theme }
    }
}

impl AppTile for DashboardUi {
    fn id(&self) -> &'static str {
        "dashboard"
    }

    fn title(&self) -> String {
        "dashboard".into()
    }

    fn pump(&mut self) -> bool {
        false
    }

    fn wants_redraw(&self) -> bool {
        true
    }

    fn handle_input(&mut self, _ev: InputEvent) -> InputResult {
        InputResult::Ignored
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Dashboard ")
            .border_style(tg_kit::ratatui::style::Style::default().fg(self.theme.panel.into()));

        let p = Paragraph::new("Dashboard placeholder").block(block);
        tg_kit::ratatui::widgets::Widget::render(p, area, buf);
    }
}

pub async fn build(
    _rt: tokio::runtime::Handle,
    on_change: ChangeHook,
    theme: Theme,
) -> Box<dyn AppTile> {
    Box::new(DashboardUi::new(Some(on_change), theme))
}
```

**Step 2: Export it and add to app slots**

Modify `crates/thegn-host/src/apps/mod.rs`:

```rust
pub mod chat;
pub mod comms;
pub mod dashboard; // <--- ADD THIS
pub mod input;
```

Modify `crates/thegn-host/src/run.rs` (around line 4547):

```rust
    let mut app_host = crate::apps::AppHost::new(vec![
        crate::apps::AppSlot::new("comms", "comms"),
        crate::apps::AppSlot::new("chat", "chat"),
        crate::apps::AppSlot::new("dashboard", "dashboard"), // <--- ADD THIS
    ]);
```

Modify `crates/thegn-host/src/run.rs` (around line 6806):

```rust
                            let tile = match app_host.slots[i].id {
                                "comms" => crate::apps::comms::build(handle, hook, theme).await,
                                "chat" => crate::apps::chat::build(handle, hook, theme).await,
                                "dashboard" => crate::apps::dashboard::build(handle, hook, theme).await, // <--- ADD THIS
                                _ => {
```

**Step 3: Test compile**
Run: `cargo check -p thegn-host`
Expected: Passes.

**Step 4: Commit**

```bash
git add crates/thegn-host/src/apps/dashboard.rs crates/thegn-host/src/apps/mod.rs crates/thegn-host/src/run.rs
git commit -m "feat(apps): add placeholder dashboard app tile"
```

---

### Task 2: Route `Action::Dashboard` Keybinding

**Objective:** Map the `Action::Dashboard` command to cycle/activate the new dashboard app tab, matching how `Action::SwitchWorkspace` or `Action::CloseTab` are dispatched in `run.rs`.

**Files:**

- Modify: `crates/thegn-host/src/run.rs`

**Step 1: Wire the action dispatch**

Search `crates/thegn-host/src/run.rs` for `crate::keymap::Action::CloseTab` and find the main action matching block (around line 6930 or wherever `forced_palette_action.take().unwrap_or(a)` resolves).

Add an arm for `Action::Dashboard`:

```rust
                crate::keymap::Action::Dashboard => {
                    // Find the index of the dashboard app.
                    let dash_idx = app_host.slots.iter().position(|s| s.id == "dashboard");
                    if let Some(i) = dash_idx {
                        if app_host.active == crate::apps::ActiveApp::Tile(i) {
                            // Toggle back to work view.
                            app_host.active = crate::apps::ActiveApp::Work;
                        } else {
                            // Switch to dashboard.
                            // Same lazy-load logic as the top-level app-tab switcher.
                            if matches!(app_host.slots[i].state, crate::apps::SlotState::Unloaded) {
                                let hook: tg_kit::ChangeHook = {
                                    let tx = app_tx.clone();
                                    let wk = waker.clone();
                                    std::sync::Arc::new(move || {
                                        let _ = tx.send(i);
                                        let _ = wk.wake();
                                    })
                                };
                                let handle = tokio::runtime::Handle::current();
                                let theme = crate::apps::kit_theme(&current_config.palette());
                                let tile = crate::apps::dashboard::build(handle, hook, theme).await;
                                app_host.slots[i].state = crate::apps::SlotState::Running(tile);
                            }
                            app_host.active = crate::apps::ActiveApp::Tile(i);
                        }
                    }
                }
```

**Step 2: Verify dispatch logic compiles**
Run: `cargo check -p thegn-host`
Expected: Passes.

**Step 3: Test execution**
Run: `cargo run -p thegn-host --bin thegn`
Test: Press `Alt+D`.
Expected: `dashboard` app tab opens and shows the placeholder paragraph. Pressing `Alt+D` again closes it.

**Step 4: Commit**

```bash
git add crates/thegn-host/src/run.rs
git commit -m "feat(apps): route Action::Dashboard to toggle dashboard tab"
```

---

### Task 3: Structure the Dashboard Layout

**Objective:** Split the dashboard view into WTF-style widgets (e.g. system info, recent repos, git status).

**Files:**

- Modify: `crates/thegn-host/src/apps/dashboard.rs`

**Step 1: Add layout and widget rendering logic**

```rust
// Update crates/thegn-host/src/apps/dashboard.rs
use tg_kit::ratatui::buffer::Buffer;
use tg_kit::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tg_kit::ratatui::widgets::{Block, Borders, Paragraph, Widget};
use tg_kit::{AppTile, ChangeHook, InputEvent, InputResult, Theme};

pub struct DashboardUi {
    theme: Theme,
}

impl DashboardUi {
    pub fn new(_on_change: Option<ChangeHook>, theme: Theme) -> Self {
        Self { theme }
    }

    fn render_system_info(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" System ")
            .border_style(tg_kit::ratatui::style::Style::default().fg(self.theme.panel2.into()));
        Paragraph::new("OS: Linux\nUptime: 2 days").block(block).render(area, buf);
    }

    fn render_repos(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Recent Repos ")
            .border_style(tg_kit::ratatui::style::Style::default().fg(self.theme.panel2.into()));
        Paragraph::new("No repos listed.").block(block).render(area, buf);
    }
}

impl AppTile for DashboardUi {
    // ... id, title, pump, wants_redraw, handle_input stay the same ...
    fn id(&self) -> &'static str { "dashboard" }
    fn title(&self) -> String { "dashboard".into() }
    fn pump(&mut self) -> bool { false }
    fn wants_redraw(&self) -> bool { true }
    fn handle_input(&mut self, _ev: InputEvent) -> InputResult { InputResult::Ignored }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        // Standard WTF-style layout: columns, then rows.
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)].as_ref())
            .split(area);

        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(0)].as_ref())
            .split(chunks[0]);

        self.render_system_info(left_chunks[0], buf);
        self.render_repos(left_chunks[1], buf);

        // Right pane placeholder
        let right_block = Block::default()
            .borders(Borders::ALL)
            .title(" Overview ")
            .border_style(tg_kit::ratatui::style::Style::default().fg(self.theme.panel.into()));
        Paragraph::new("Welcome to the Thegn Dashboard").block(right_block).render(chunks[1], buf);
    }
}

// build function stays the same...
```

**Step 2: Test compile**
Run: `cargo check -p thegn-host`
Expected: Passes.

**Step 3: Commit**

```bash
git add crates/thegn-host/src/apps/dashboard.rs
git commit -m "feat(apps): implement standard wtf-style dashboard grid layout"
```

---

### Task 4: Hook Up Live Data (Recents / Metrics)

**Objective:** Fetch real data from `thegn-core` (like `Db::known_repos()` or `sysinfo`) and trigger redraws safely from background tasks via `ChangeHook`.

**Files:**

- Modify: `crates/thegn-host/src/apps/dashboard.rs`

**Step 1: Integrate `sysinfo` and `thegn_core::db` fetching**

```rust
// Update crates/thegn-host/src/apps/dashboard.rs
use std::sync::{Arc, Mutex};
use tg_kit::ratatui::buffer::Buffer;
use tg_kit::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tg_kit::ratatui::widgets::{Block, Borders, Paragraph, Widget};
use tg_kit::{AppTile, ChangeHook, InputEvent, InputResult, Theme};

#[derive(Clone, Default)]
struct DashboardData {
    pub recent_repos: Vec<String>,
    pub sys_info: String,
}

pub struct DashboardUi {
    theme: Theme,
    data: Arc<Mutex<DashboardData>>,
}

impl DashboardUi {
    pub fn new(rt: tokio::runtime::Handle, on_change: Option<ChangeHook>, theme: Theme) -> Self {
        let data = Arc::new(Mutex::new(DashboardData::default()));

        let data_clone = data.clone();
        rt.spawn(async move {
            // Initial data fetch
            let mut repos = vec![];
            if let Ok(db) = thegn_core::db::Db::open() {
                if let Ok(recent) = db.recent_repos(10) {
                    repos = recent;
                }
            }

            let mut sys = sysinfo::System::new_all();
            sys.refresh_all();
            let mem_gb = sys.used_memory() as f64 / 1_073_741_824.0;
            let total_gb = sys.total_memory() as f64 / 1_073_741_824.0;

            let info = format!("OS: {}\nUptime: {}s\nMem: {:.2}GB / {:.2}GB",
                sysinfo::System::long_os_version().unwrap_or_default(),
                sysinfo::System::uptime(),
                mem_gb, total_gb
            );

            {
                let mut d = data_clone.lock().unwrap();
                d.recent_repos = repos;
                d.sys_info = info;
            }

            if let Some(hook) = on_change {
                hook();
            }
        });

        Self { theme, data }
    }

    // update render functions to use self.data.lock().unwrap()
}
// ...
```

**Note:** Ensure `sysinfo` dependency handles are valid in `Cargo.toml`. `sysinfo` is already present in `thegn-core`. You may need to add it to `thegn-host` dependencies if not already inherited or exported.

**Step 2: Commit**

```bash
git commit -am "feat(apps): wire real data fetching for dashboard widgets"
```

**Final Verification:**
Run the interactive compositor and open the dashboard tab to visually confirm styling, bounds, and layout matching.
