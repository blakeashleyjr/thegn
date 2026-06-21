# Agent Harness Side Panel — Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Add a new "Agent" accordion section to superzej's side panel that exposes real-time data from AI coding agent harnesses (Claude Code, Codex CLI, OpenCode, Hermes, PI, Antigravity), replicating all abtop data plus additional metrics.

**Architecture:** Two-phase approach: (1) Create `superzej-core` crate with agent session collection + serialization types, (2) Expose via plugin API / side panel using existing PanelData infrastructure. Reuse existing hydration thread for periodic collection.

**Tech Stack:** Rust (existing superzej codebase), serde for JSON, existing process/port discovery from abtop (adapted).

---

## Background: abtop Data Model

The [abtop](https://github.com/graykode/abtop) project exposes:

### Per-Session Fields (AgentSession)

- `agent_cli`: "claude", "codex", "opencode"
- `pid`: OS process ID
- `session_id`: Agent-assigned session ID
- `cwd`: Working directory
- `project_name`: Basename of cwd
- `started_at`: Unix-epoch ms
- `status`: Thinking | Executing | Waiting | Unknown | RateLimited | Done
- `model`: Model identifier (e.g. "claude-opus-4-6")
- `effort`: Reasoning effort (Codex only: minimal|low|medium|high)
- `context_percent`: 0.0–100.0%
- `context_window`: Total tokens (e.g. 200000)
- `total_input_tokens`, `total_output_tokens`, `total_cache_read`, `total_cache_create`
- `turn_count`: User/assistant turns
- `current_tasks`: Vec<String> of active tasks
- `mem_mb`: Resident memory in MiB
- `version`: Agent CLI version
- `git_branch`, `git_added`, `git_modified`: Git status
- `token_history`: Vec<u64> per-turn tokens
- `context_history`: Vec<u64> per-turn context sizes
- `compaction_count`: Context compaction events
- `subagents`: Vec<SubAgent> (name, status, tokens)
- `mem_file_count`, `mem_line_count`: Memory stats (Claude only)
- `children`: Vec<ChildProcess> with pid, command, mem_kb, port
- `initial_prompt`: First user prompt (truncated)
- `first_assistant_text`: First response text
- `chat_messages`: Vec<ChatMessage> (role, text)
- `tool_calls`: Vec<ToolCall> (name, arg, duration_ms)
- `pending_since_ms`: Unix ms of in-flight tools
- `thinking_since_ms`: Unix ms of thinking state
- `file_accesses`: Vec<FileAccess> (path, operation: Read|Write|Edit)
- `config_root`: Home-abbreviated config dir (~/.claude, ~/.codex)

### Aggregate / System Fields

- `RateLimitInfo`: source, five_hour_pct, five_hour_resets_at, seven_day_pct, seven_day_resets_at, updated_at
- `OrphanPort`: port, pid, command, project_name
- `McpServer`: pid, parent_cli, profile, mem_kb, rollouts (path, mtime, size_bytes)
- `HostMetrics`: cpu_pct, mem_pct, load1
- `AgentAggregate`: mem_mb, avg_ctx_pct, active_count

---

## Phase 1: Core Agent Data Collection (superzej-core)

### Task 1: Create Agent Data Types in superzej-core

**Objective:** Define serializable types for agent session data in `superzej-core`.

**Files:**

- Create: `crates/superzej-core/src/agent.rs` (new file)

**Step 1: Define core enums and structs**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Thinking,
    Executing,
    Waiting,
    Unknown,
    RateLimited,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildProcess {
    pub pid: u32,
    pub command: String,
    pub mem_kb: u64,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgent {
    pub name: String,
    pub status: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub arg: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String, // "user" or "assistant"
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileOp {
    Read,
    Write,
    Edit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAccess {
    pub path: String,
    pub operation: FileOp,
    pub turn_index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub agent_cli: String,
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub project_name: String,
    pub started_at: u64,
    pub status: SessionStatus,
    pub model: String,
    pub effort: String,
    pub context_percent: f64,
    pub context_window: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read: u64,
    pub total_cache_create: u64,
    pub turn_count: u32,
    pub current_tasks: Vec<String>,
    pub mem_mb: u64,
    pub version: String,
    pub git_branch: String,
    pub git_added: u32,
    pub git_modified: u32,
    pub token_history: Vec<u64>,
    pub context_history: Vec<u64>,
    pub compaction_count: u32,
    pub subagents: Vec<SubAgent>,
    pub mem_file_count: u32,
    pub mem_line_count: u32,
    pub children: Vec<ChildProcess>,
    pub initial_prompt: String,
    pub first_assistant_text: String,
    pub chat_messages: Vec<ChatMessage>,
    pub tool_calls: Vec<ToolCall>,
    pub pending_since_ms: u64,
    pub thinking_since_ms: u64,
    pub file_accesses: Vec<FileAccess>,
    pub config_root: String,
}

impl AgentSession {
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
            + self.total_cache_read + self.total_cache_create
    }
}
```

**Step 2: Add rate limit and system types**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfo {
    pub source: String,
    pub five_hour_pct: Option<f64>,
    pub five_hour_resets_at: Option<u64>,
    pub seven_day_pct: Option<f64>,
    pub seven_day_resets_at: Option<u64>,
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanPort {
    pub port: u16,
    pub pid: u32,
    pub command: String,
    pub project_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostMetrics {
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub load1: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAggregate {
    pub mem_mb: u64,
    pub avg_ctx_pct: f64,
    pub active_count: usize,
}
```

**Step 3: Run test to verify compilation**

Run: `cargo build -p superzej-core`
Expected: SUCCESS

---

### Task 2: Implement Claude Collector

**Objective:** Parse Claude Code session data from `~/.claude/sessions/*.json`.

**Files:**

- Modify: `crates/superzej-core/src/agent.rs` (add collector code)

**Step 1: Add ClaudeCollector struct**

```rust
use std::path::PathBuf;
use std::collections::HashMap;

pub struct ClaudeCollector {
    config_dirs: Vec<PathBuf>,
    transcript_cache: HashMap<String, TranscriptResult>,
}

struct ConfigDir {
    sessions_dir: PathBuf,
    projects_dir: PathBuf,
}

struct TranscriptResult {
    // ... cached parse results
}
```

**Step 2: Implement session discovery**

- Scan `~/.claude/sessions/` for `*.json` files
- Match running `claude` PIDs to session files via `/proc/<pid>/fd`
- Parse JSON for tokens, model, context, tools

**Step 3: Run test**

Run: `cargo test -p superzej-core agent::claude`
Expected: Tests pass (write unit tests for parsing)

---

### Task 3: Implement Codex Collector

**Objective:** Parse Codex CLI session data from `~/.codex/sessions/rollout-*.jsonl`.

**Files:**

- Modify: `crates/superzej-core/src/agent.rs`

**Step 1: Add CodexCollector**

```rust
pub struct CodexCollector {
    sessions_dir: PathBuf,
}
```

**Step 2: Implement JSONL parsing**

- Parse `rollout-*.jsonl` files
- Extract session_meta, event_msg, response_item events
- Map tokens, model, effort, git info

---

### Task 4: Implement OpenCode Collector

**Objective:** Query OpenCode SQLite database at `~/.local/share/opencode/opencode.db`.

**Files:**

- Modify: `crates/superzej-core/src/agent.rs`

**Step 1: Add OpenCodeCollector**

```rust
pub struct OpenCodeCollector {
    db_path: PathBuf,
    cached_db_sessions: Vec<DbSession>,
}
```

**Step 2: Query SQLite**

- Use `sqlite3` CLI for safe concurrent reads
- Match running PIDs to sessions by cwd

---

### Task 5: Process & Port Discovery

**Objective:** Reuse process/port scanning logic adapted from abtop.

**Files:**

- Modify: `crates/superzej-core/src/agent.rs`

**Step 1: Add process info types**

```rust
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub rss_kb: u64,
    pub cpu_pct: f64,
    pub command: String,
}
```

**Step 2: Implement platform-specific采集**

- Linux: Parse `/proc/*/stat`, `/proc/*/cmdline`
- macOS: Use `ps` command
- Windows: Use `sysinfo` crate

**Step 3: Add port discovery**

- Linux: Scan `/proc/<pid>/fd` for socket inodes
- Parse `/proc/net/tcp`, `/proc/net/tcp6`

---

## Phase 2: Panel Integration (superzej-host)

### Task 6: Add Agent Section to Panel

**Objective:** Add "agent" as a new accordion section.

**Files:**

- Modify: `crates/superzej-host/src/panel/mod.rs`

**Step 1: Add Section variant**

```rust
pub enum Section {
    // ... existing
    Agent,  // Add this
}
```

**Step 2: Update SECTION_ORDER**

```rust
pub const SECTION_ORDER: [Section; 13] = [
    Section::Changes,
    Section::Commits,
    Section::Branches,
    Section::Stash,
    Section::Git,
    Section::Files,
    Section::Tests,
    Section::Debug,
    Section::Sandbox,
    Section::Db,
    Section::Telemetry,
    Section::Keys,
    Section::Agent,  // Add
];
```

---

### Task 7: Add Agent Data to PanelData

**Objective:** Store agent sessions in the panel data model.

**Files:**

- Modify: `crates/superzej-host/src/panel/mod.rs`

**Step 1: Extend PanelData**

```rust
pub struct PanelData {
    // ... existing fields
    pub agent_sessions: Vec<superzej_core::agent::AgentSession>,
    pub agent_aggregate: superzej_core::agent::AgentAggregate,
    pub rate_limits: Vec<superzej_core::agent::RateLimitInfo>,
    pub orphan_ports: Vec<superzej_core::agent::OrphanPort>,
}
```

---

### Task 8: Implement Agent Section Renderer

**Objective:** Render the agent section content.

**Files:**

- Create: `crates/superzej-host/src/panel/sections/agent.rs`
- Modify: `crates/superzej-host/src/panel/sections/mod.rs`

**Step 1: Create section content**

```rust
use superzej_core::agent::{AgentSession, AgentAggregate, RateLimitInfo};
use crate::seg::{Line, Seg, Tok, seg, sp};
use super::{PanelRow, SectionCtx, PanelHit};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let mut rows = Vec::new();

    // Aggregate summary
    let agg = &data.agent_aggregate;
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(Tok::Slot(S::Accent), format!("{} active", agg.active_count)),
        seg(Tok::Slot(S::Ghost), format!(" · {} MB", agg.mem_mb)),
    ])));

    // Sessions list
    for (i, session) in data.agent_sessions.iter().enumerate() {
        rows.push(agent_session_row(session, i));
    }

    rows
}

fn agent_session_row(session: &AgentSession, idx: usize) -> PanelRow {
    // Render: agent glyph, project, status, tokens, context %
    // ...
    PanelRow::plain(Line::segs(segs)).with_hit(PanelHit::Row(Section::Agent, idx))
}
```

**Step 2: Add summary function**

```rust
pub fn summary(section: Section, model: &crate::chrome::FrameModel) -> Vec<Seg> {
    match section {
        Section::Agent => {
            let agg = &model.panel.agent_aggregate;
            vec![
                seg(hue(Hue::Purple), format!("{}", agg.active_count)),
                seg(g(), " active"),
            ]
        }
        // ... other sections
    }
}
```

---

### Task 9: Hook Into Hydration

**Objective:** Run agent collection on the hydration thread.

**Files:**

- Modify: `crates/superzej-host/src/hydrate.rs`

**Step 1: Add agent collection to model refresh**

```rust
fn refresh_panel_data(
    // ... existing params
    agent_sessions: Vec<superzej_core::agent::AgentSession>,
) -> PanelData {
    let mut panel = existing_panel_data;

    panel.agent_sessions = agent_sessions;
    panel.agent_aggregate = AgentAggregate::from_sessions(&panel.agent_sessions);

    panel
}
```

---

### Task 10: Config for Agent Panel

**Objective:** Allow users to configure agent collection.

**Files:**

- Modify: `crates/superzej-core/src/config.rs`

**Step 1: Add agent config**

```rust
pub struct AgentConfig {
    pub enabled: bool,
    pub hidden_agents: Vec<String>,  // e.g. ["codex"]
    pub refresh_interval_secs: f64,
    pub show_rate_limits: bool,
    pub show_orphan_ports: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hidden_agents: vec![],
            refresh_interval_secs: 2.0,
            show_rate_limits: true,
            show_orphan_ports: true,
        }
    }
}
```

---

## Phase 3: Extended Agent Support (Beyond abtop)

### Task 11: Hermes Agent Support

**Objective:** Collect Hermes agent data (from Hermes Kanban / task system).

**Files:**

- Modify: `crates/superzej-core/src/agent.rs`

**Step 1: Add HermesCollector**

```rust
/// Hermes-specific session data from superzej's own DB/activity system
pub struct HermesCollector {
    // Read from superzej's SQLite DB for active worktree agent assignments
}
```

**Step 2: Map to AgentSession**

- Use `db.worktree_agent()` to get agent assignments
- Get activity state from `activity::read_states()`

---

### Task 12: PI / Antigravity Support

**Objective:** Support PI (Private Intelligence) and Antigravity agents.

**Files:**

- Modify: `crates/superzej-core/src/agent.rs`

**Step 1: Add generic collector interface**

```rust
pub trait AgentCollector {
    fn collect(&mut self) -> Vec<AgentSession>;
    fn name(&self) -> &'static str;
}
```

**Step 2: Implement PI collector**

- PI typically stores sessions in `~/.pi/sessions/`
- Parse similar JSON format

**Step 3: Implement Antigravity collector**

- Antigravity uses custom data directory
- Parse agent-specific format

---

## Verification

### Test 1: Build Compilation

```bash
cargo build --workspace
```

### Test 2: Unit Tests

```bash
cargo test -p superzej-core agent
cargo test -p superzej-host panel::sections::agent
```

### Test 3: Manual Test

```bash
# Run superzej and verify "agent" section appears in panel
just dev
# Press key to open agent section (if mapped)
```

---

## Risks & Tradeoffs

1. **Performance:** Agent collection runs on hydration thread; ensure <100ms per tick. Cache expensive operations (transcript parsing) across ticks.

2. **Privacy:** Agent sessions may contain sensitive data. Add redaction (like abtop does) for secrets in chat/tool messages.

3. **Platform:** Process/port scanning has platform-specific code. Test on Linux, macOS, Windows.

4. **Rate Limits:** Claude rate limits require `--print` call which consumes quota. Make optional or cache aggressively.

---

## Open Questions

1. **Should agent panel show ALL sessions or only those in the current workspace?** — abtop shows global; superzej may benefit from workspace-scoped view.

2. **How to handle Hermes agents specifically?** — Hermes is the host itself; should we show internal agent state differently?

3. **Real-time updates?** — Agent data changes on each user turn; 2s refresh may be too slow for token counts.

4. **MCP server display?** — Include MCP servers like abtop does, or simplify?

---

## Files Likely to Change

- `crates/superzej-core/src/agent.rs` (new)
- `crates/superzej-core/src/lib.rs` (add `pub mod agent`)
- `crates/superzej-core/src/config.rs` (add AgentConfig)
- `crates/superzej-host/src/panel/mod.rs` (add Section::Agent, PanelData fields)
- `crates/superzej-host/src/panel/sections/agent.rs` (new)
- `crates/superzej-host/src/panel/sections/mod.rs` (register agent section)
- `crates/superzej-host/src/hydrate.rs` (hook agent collection)
