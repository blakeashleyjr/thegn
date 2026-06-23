//! The `agent` app tab — termite-agent's autonomous coding runtime hosted
//! inside superzej as a first-class top-level tab (mirrors [`super::chat`]).
//!
//! termite-core's [`AgentRuntime`] and its `OpenAICompatibleProvider` are
//! *blocking* (`reqwest::blocking`) and tool execution is synchronous, so a
//! turn runs on a `spawn_blocking` worker and folds back via the [`ChangeHook`]
//! and an internal channel — the same ~0%-idle pattern as the chat tile.
//! [`AppTile::handle_input`] never blocks or awaits.
//!
//! What this tile wires into the superzej substrate:
//! - **Proxy (Phase B):** when `[llm_proxy]` is enabled, the model path points
//!   at the local `szproxy` and a per-worktree *scoped virtual key* is minted
//!   and used as the provider key — the master key never reaches the harness and
//!   spend is attributed per worktree. The key is revoked on tab teardown.
//! - **Sandbox (Phase C):** the `terminal` tool runs arbitrary commands, so it
//!   executes through the worktree's sandbox (`sandbox::enter_argv`) — the
//!   policy boundary pi/termite deliberately omit. `read`/`write`/`search` use
//!   termite's built-ins scoped to the worktree.
//! - **Notifications/observability (Phase D):** turn completion/failure publish
//!   `AgentDone`/`AgentFailed` into the host event bus (priority model + toast),
//!   and the tab chip + status line surface the working state and turn count.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::event_bus::{Event, EventBus};
use superzej_core::remote::GitLoc;
use superzej_core::{repo, sandbox};
use sz_kit::input::{InputEvent, InputResult, Key, Modifiers};
use sz_kit::ratatui::buffer::Buffer;
use sz_kit::ratatui::layout::{Constraint, Direction, Layout, Rect};
use sz_kit::ratatui::style::{Modifier, Style};
use sz_kit::ratatui::text::{Line, Span};
use sz_kit::ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use sz_kit::{AppTile, ChangeHook, Rgb, Theme};
use termite_core::{
    AgentRuntime, OpenAICompatibleProvider, ProviderClient, ProviderError, ProviderResponse,
    ProviderResult, Tool, ToolContext, ToolError, ToolExecution, ToolRegistry,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

/// Cap on autonomous tool-call iterations per turn (guards runaway loops).
const MAX_ITERATIONS: usize = 16;

/// Build the `agent` tile. Mirrors [`super::chat::build`]'s signature plus the
/// config (to route the provider through the proxy and scope tools/sandbox to
/// the active worktree) and the host event bus (to emit agent notifications).
/// The tab always opens; a missing/blocked model surfaces as an in-transcript
/// error on the first turn (chat parity).
pub async fn build(
    rt: Handle,
    on_change: ChangeHook,
    theme: Theme,
    cfg: &Config,
    event_bus: Option<EventBus>,
) -> Box<dyn AppTile> {
    // The agent operates from the directory szhost was launched in — its active
    // worktree. Tools and the sandbox are scoped to it.
    let worktree = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let tools = build_tool_registry(cfg, &worktree);
    let (proxy_key_id, provider) = build_provider(cfg, &worktree);
    let runtime = AgentRuntime::new(tools, provider);
    Box::new(AgentUi::new(
        runtime,
        rt,
        on_change,
        theme,
        worktree,
        event_bus,
        proxy_key_id,
    ))
}

/// Construct the provider for the embedded harness, returning the minted proxy
/// virtual-key id (if any) so the tile can revoke it on teardown.
///
/// When `[llm_proxy]` is enabled, the OpenAI-compatible base URL points at the
/// local `szproxy` (so cost/limit/failover/token-reduction apply) and a
/// per-worktree scoped virtual key is minted as the provider key — the proxy
/// maps it to the real upstream, so the master key never reaches the harness.
/// Falls back to the env key, then to a clear "unconfigured" provider so the
/// tab still opens.
fn build_provider(cfg: &Config, worktree: &str) -> (Option<String>, Box<dyn ProviderClient>) {
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
    if cfg.llm_proxy.enabled {
        let base = format!("http://{}/v1", cfg.llm_proxy.listen);
        if let Some(key) = mint_proxy_key(worktree) {
            return (
                Some(key.clone()),
                Box::new(OpenAICompatibleProvider::new(base, model, key)),
            );
        }
        // Proxy on but key mint failed (no DB) — still route through the proxy
        // with whatever env key is present.
        let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
        return (
            None,
            Box::new(OpenAICompatibleProvider::new(base, model, key)),
        );
    }
    match OpenAICompatibleProvider::from_env() {
        Ok(p) => (None, Box::new(p)),
        Err(_) => (None, Box::new(UnconfiguredProvider)),
    }
}

/// Mint a worktree-scoped proxy virtual key. The proxy authenticates by the
/// bearer token (which is the key id) and resolves it to a scope for budget
/// attribution; we store the token as its own hash since the lookup is by id.
/// Returns `None` (no key) if the state DB can't be opened.
fn mint_proxy_key(worktree: &str) -> Option<String> {
    let db = Db::open().ok()?;
    let token = random_token();
    let scope = format!("worktree:{worktree}");
    let label = format!("embedded agent (termite) — {worktree}");
    db.put_proxy_virtual_key(&token, &token, &label, &scope, None, now_ms())
        .ok()?;
    Some(token)
}

/// A random, hard-to-guess token for a proxy virtual key (128 bits of urandom,
/// hex). Falls back to a zeroed token if `/dev/urandom` is unreadable.
fn random_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    let mut s = String::from("szk-");
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build the coding-tool registry: termite's `read`/`write`/`search` scoped to
/// the worktree, plus a sandbox-wrapped `terminal`.
fn build_tool_registry(cfg: &Config, worktree: &str) -> ToolRegistry {
    let workdir = PathBuf::from(worktree);
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(termite_core::tool::ReadFileTool::new(
        workdir.clone(),
    )));
    reg.register(Box::new(termite_core::tool::WriteFileTool::new(
        workdir.clone(),
    )));
    reg.register(Box::new(termite_core::tool::SearchFilesTool::new(
        workdir.clone(),
    )));
    // `terminal` runs arbitrary commands — route it through the worktree's
    // sandbox (the policy boundary pi/termite omit). `None` spec → host shell.
    let spec = resolve_tool_sandbox(cfg, worktree);
    reg.register(Box::new(SandboxTerminalTool { spec, workdir }));
    reg
}

/// Resolve (and `ensure`) the worktree's sandbox spec for tool execution,
/// reusing the pane launch path so the agent's tools run in the same containment
/// as an interactive pane would. Includes the ssh-config shim so git-over-ssh
/// works from the `terminal` tool. `None` when sandboxing is disabled/unavailable.
fn resolve_tool_sandbox(cfg: &Config, worktree: &str) -> Option<sandbox::SandboxSpec> {
    let loc = GitLoc::from_db(worktree, None);
    let repo_root = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let mut spec = crate::agent::prepare_sandbox(cfg, &repo_root, worktree, &loc, None, false)
        .ok()?
        .spec?;
    crate::agent::apply_ssh_config_shim(&mut spec);
    Some(spec)
}

/// termite's `terminal` tool, re-implemented to run the command inside the
/// worktree's sandbox (`sandbox::enter_argv`) instead of a bare host shell.
/// Same name + schema as termite's built-in, so it's a drop-in replacement.
struct SandboxTerminalTool {
    spec: Option<sandbox::SandboxSpec>,
    workdir: PathBuf,
}

impl Tool for SandboxTerminalTool {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn schema_json(&self) -> String {
        r#"{"name": "terminal", "description": "Executes shell commands", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}"#.to_string()
    }

    fn execute(
        &self,
        input_json: &str,
        _ctx: &mut ToolContext,
    ) -> Result<ToolExecution, ToolError> {
        let args: serde_json::Value = serde_json::from_str(input_json)
            .map_err(|err| ToolError::Execution(format!("Invalid json: {err}")))?;
        let command = args["command"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("Missing command".into()))?;
        let argv = match &self.spec {
            Some(spec) => sandbox::enter_argv(spec, command),
            None => vec!["bash".to_string(), "-c".to_string(), command.to_string()],
        };
        let (prog, rest) = argv
            .split_first()
            .ok_or_else(|| ToolError::Execution("empty sandbox argv".into()))?;
        let output = std::process::Command::new(prog)
            .args(rest)
            .current_dir(&self.workdir)
            .output()
            .map_err(|err| ToolError::Execution(format!("Failed to execute command: {err}")))?;
        let mut out = String::from_utf8_lossy(&output.stdout).into_owned();
        let err = String::from_utf8_lossy(&output.stderr);
        if !err.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&err);
        }
        Ok(ToolExecution::new(out))
    }
}

/// Stand-in provider used when no model credentials are configured. Returns a
/// descriptive error the moment a turn is attempted, so the tab opens cleanly
/// and the user learns what to set instead of the tile failing to construct.
struct UnconfiguredProvider;

impl ProviderClient for UnconfiguredProvider {
    fn complete_once(
        &self,
        _conversation: &[termite_core::Message],
    ) -> ProviderResult<ProviderResponse> {
        Err(ProviderError::Unavailable(
            "no model configured — set OPENAI_API_KEY (and OPENAI_BASE_URL/OPENAI_MODEL), \
             or enable [llm_proxy] in superzej config"
                .to_string(),
        ))
    }
}

/// One rendered transcript line, tagged by speaker for styling.
enum Entry {
    User(String),
    Assistant(String),
    Tool(String),
    Error(String),
}

/// Result of an autonomous turn, posted from the `spawn_blocking` worker.
enum TurnEvent {
    Done {
        response: String,
        events: Vec<String>,
    },
    Failed(String),
}

/// The embeddable agent tile.
pub struct AgentUi {
    runtime: Arc<Mutex<AgentRuntime>>,
    transcript: Vec<Entry>,
    input: String,
    busy: bool,
    /// Completed turns this session (observability).
    turns: u32,
    /// The agent's active worktree (notification target + status line).
    worktree: String,
    /// Host event bus for `AgentDone`/`AgentFailed` notifications, if wired.
    event_bus: Option<EventBus>,
    /// Minted proxy virtual-key id, revoked on teardown.
    proxy_key_id: Option<String>,
    /// Cached proxy spend (tokens, USD) for this worktree's budget scope,
    /// refreshed after each turn. `None` unless routed through the proxy.
    spend: Option<(i64, f64)>,
    rt: Handle,
    tx: mpsc::UnboundedSender<TurnEvent>,
    rx: mpsc::UnboundedReceiver<TurnEvent>,
    on_change: ChangeHook,
    theme: Theme,
    dirty: bool,
}

impl AgentUi {
    #[allow(clippy::too_many_arguments)]
    fn new(
        runtime: AgentRuntime,
        rt: Handle,
        on_change: ChangeHook,
        theme: Theme,
        worktree: String,
        event_bus: Option<EventBus>,
        proxy_key_id: Option<String>,
    ) -> AgentUi {
        let (tx, rx) = mpsc::unbounded_channel();
        AgentUi {
            runtime: Arc::new(Mutex::new(runtime)),
            transcript: Vec::new(),
            input: String::new(),
            busy: false,
            turns: 0,
            worktree,
            event_bus,
            proxy_key_id,
            spend: None,
            rt,
            tx,
            rx,
            on_change,
            theme,
            dirty: true,
        }
    }

    /// Kick off an autonomous turn for the current input on a blocking worker.
    fn submit(&mut self) {
        let prompt = std::mem::take(&mut self.input);
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        self.transcript.push(Entry::User(prompt.clone()));
        self.busy = true;

        let runtime = Arc::clone(&self.runtime);
        let tx = self.tx.clone();
        let hook = self.on_change.clone();
        self.rt.spawn_blocking(move || {
            // One turn at a time (the `busy` guard blocks concurrent submits),
            // so holding the lock across the blocking provider call is fine.
            let ev = match runtime.lock() {
                Ok(mut rt) => match rt.run_autonomous_turn(&prompt, MAX_ITERATIONS) {
                    Ok(out) => TurnEvent::Done {
                        response: out.response,
                        events: out.events,
                    },
                    Err(err) => TurnEvent::Failed(err.to_string()),
                },
                Err(_) => TurnEvent::Failed("agent runtime lock poisoned".to_string()),
            };
            if tx.send(ev).is_ok() {
                hook();
            }
        });
    }

    fn apply_event(&mut self, ev: TurnEvent) {
        match ev {
            TurnEvent::Done { response, events } => {
                for e in events {
                    self.transcript.push(Entry::Tool(e));
                }
                if !response.trim().is_empty() {
                    self.transcript.push(Entry::Assistant(response));
                }
                self.busy = false;
                self.turns += 1;
                self.refresh_spend();
                self.notify_turn(true, None);
            }
            TurnEvent::Failed(msg) => {
                self.transcript.push(Entry::Error(msg.clone()));
                self.busy = false;
                self.refresh_spend();
                self.notify_turn(false, Some(msg));
            }
        }
    }

    /// Refresh cached proxy spend for this worktree's budget scope. Cheap SQLite
    /// read; called per turn (not per frame). No-op unless a virtual key was
    /// minted (i.e. traffic is routed through the proxy).
    fn refresh_spend(&mut self) {
        if self.proxy_key_id.is_none() {
            return;
        }
        let scope = format!("worktree:{}", self.worktree);
        if let Ok(db) = Db::open()
            && let Ok(Some(b)) = db.proxy_budget(&scope)
        {
            self.spend = Some((b.spent_tokens, b.spent_cost));
        }
    }

    /// Publish an `AgentDone`/`AgentFailed` event so the host's priority model
    /// raises the right notification (badge/toast). No-op without an event bus.
    fn notify_turn(&self, success: bool, error: Option<String>) {
        let Some(bus) = &self.event_bus else {
            return;
        };
        let event = if success {
            Event::AgentDone {
                worktree: self.worktree.clone(),
                agent: "termite".to_string(),
                success: true,
            }
        } else {
            Event::AgentFailed {
                worktree: self.worktree.clone(),
                agent: "termite".to_string(),
                error: error.unwrap_or_default(),
            }
        };
        bus.publish_with_notification(&event);
    }

    fn color(&self, rgb: Rgb) -> sz_kit::ratatui::style::Color {
        sz_kit::ratatui::style::Color::Rgb(rgb.0, rgb.1, rgb.2)
    }

    fn transcript_lines(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        for entry in &self.transcript {
            let (label, body, color) = match entry {
                Entry::User(t) => ("you", t.as_str(), self.theme.accent),
                Entry::Assistant(t) => ("agent", t.as_str(), self.theme.text),
                Entry::Tool(t) => ("tool", t.as_str(), self.theme.teal),
                Entry::Error(t) => ("error", t.as_str(), self.theme.red),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{label}  "),
                    Style::default()
                        .fg(self.color(color))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    body.to_string(),
                    Style::default().fg(self.color(self.theme.text)),
                ),
            ]));
        }
        if self.transcript.is_empty() {
            lines.push(Line::from(Span::styled(
                "Ask the embedded agent to read, edit, search, or run commands in this worktree.",
                Style::default().fg(self.color(self.theme.dim)),
            )));
        }
        lines
    }

    fn worktree_label(&self) -> &str {
        Path::new(&self.worktree)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(self.worktree.as_str())
    }
}

impl AppTile for AgentUi {
    fn id(&self) -> &'static str {
        "agent"
    }

    fn title(&self) -> String {
        if self.busy {
            "agent ●".to_string()
        } else {
            "agent".to_string()
        }
    }

    fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.rx.try_recv() {
            changed = true;
            self.apply_event(ev);
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    fn wants_redraw(&self) -> bool {
        self.dirty
    }

    fn handle_input(&mut self, ev: InputEvent) -> InputResult {
        let result = match ev {
            InputEvent::Key { key, modifiers } => self.handle_key(key, modifiers),
            InputEvent::Paste(text) => {
                self.input.push_str(&text);
                InputResult::Consumed
            }
            InputEvent::Resize(..) => InputResult::Consumed,
            InputEvent::Tick => InputResult::Ignored,
        };
        if result == InputResult::Consumed {
            self.dirty = true;
        }
        result
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.dirty = false;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(area);

        let transcript = Paragraph::new(self.transcript_lines())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.color(self.theme.border)))
                    .title(" agent "),
            );
        buf.set_style(chunks[0], Style::default().bg(self.color(self.theme.bg0)));
        transcript.render(chunks[0], buf);

        let input = Paragraph::new(format!("{}▌", self.input)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(self.color(if self.busy {
                    self.theme.dim
                } else {
                    self.theme.focus
                })))
                .title(if self.busy {
                    " working… "
                } else {
                    " message "
                }),
        );
        input.render(chunks[1], buf);
    }

    fn status_line(&self) -> Option<String> {
        let wt = self.worktree_label();
        let spend = match self.spend {
            Some((tokens, cost)) => format!(" · {tokens} tok ${cost:.4}"),
            None => String::new(),
        };
        Some(if self.busy {
            format!(
                "agent[{wt}]: working — turn {} in flight{spend}",
                self.turns + 1
            )
        } else {
            format!(
                "agent[{wt}]: {} turn(s){spend} · Enter to send · Esc to leave tab",
                self.turns
            )
        })
    }

    fn shutdown(&mut self) {
        // Revoke the minted proxy virtual key so it can't be reused after the
        // tab closes.
        if let Some(key) = self.proxy_key_id.take()
            && let Ok(db) = Db::open()
        {
            let _ = db.revoke_proxy_virtual_key(&key, now_ms());
        }
    }
}

impl AgentUi {
    fn handle_key(&mut self, key: Key, mods: Modifiers) -> InputResult {
        match key {
            Key::Escape => {
                if self.busy {
                    // No mid-turn cancellation (the provider call is blocking
                    // and not abortable); swallow the key.
                    InputResult::Consumed
                } else {
                    InputResult::Exit
                }
            }
            Key::Char('c') if mods.ctrl => {
                if self.busy {
                    InputResult::Consumed
                } else {
                    InputResult::Exit
                }
            }
            Key::Enter if !mods.alt => {
                if !self.busy {
                    self.submit();
                }
                InputResult::Consumed
            }
            Key::Backspace => {
                self.input.pop();
                InputResult::Consumed
            }
            Key::Char(c) => {
                self.input.push(c);
                InputResult::Consumed
            }
            _ => InputResult::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ui() -> AgentUi {
        let runtime = AgentRuntime::new(ToolRegistry::new(), Box::new(UnconfiguredProvider));
        let hook: ChangeHook = Arc::new(|| {});
        AgentUi::new(
            runtime,
            Handle::current(),
            hook,
            Theme::prism(),
            "/tmp/wt".to_string(),
            None,
            None,
        )
    }

    #[tokio::test]
    async fn typing_and_backspace_edit_the_input() {
        let mut ui = test_ui();
        for c in "hello".chars() {
            ui.handle_input(InputEvent::key(Key::Char(c)));
        }
        assert_eq!(ui.input, "hello");
        ui.handle_input(InputEvent::key(Key::Backspace));
        assert_eq!(ui.input, "hell");
    }

    #[tokio::test]
    async fn submit_pushes_a_user_entry_and_marks_busy() {
        let mut ui = test_ui();
        ui.input = "do something".to_string();
        ui.submit();
        assert!(ui.busy);
        assert!(matches!(ui.transcript.first(), Some(Entry::User(t)) if t == "do something"));
        // The unconfigured provider fails the turn; draining yields an error
        // entry and clears busy.
        let ev = ui.rx.recv().await.expect("turn event");
        ui.apply_event(ev);
        assert!(!ui.busy);
        assert!(matches!(ui.transcript.last(), Some(Entry::Error(_))));
    }

    #[tokio::test]
    async fn empty_submit_is_a_noop() {
        let mut ui = test_ui();
        ui.input = "   ".to_string();
        ui.submit();
        assert!(!ui.busy);
        assert!(ui.transcript.is_empty());
    }

    #[tokio::test]
    async fn escape_exits_when_idle_but_not_while_busy() {
        let mut ui = test_ui();
        assert_eq!(
            ui.handle_key(Key::Escape, Modifiers::NONE),
            InputResult::Exit
        );
        ui.busy = true;
        assert_eq!(
            ui.handle_key(Key::Escape, Modifiers::NONE),
            InputResult::Consumed
        );
    }

    #[tokio::test]
    async fn done_and_failed_turns_publish_agent_events() {
        let bus = EventBus::new();
        let sub = bus.subscribe();
        let runtime = AgentRuntime::new(ToolRegistry::new(), Box::new(UnconfiguredProvider));
        let hook: ChangeHook = Arc::new(|| {});
        let mut ui = AgentUi::new(
            runtime,
            Handle::current(),
            hook,
            Theme::prism(),
            "/tmp/wt".to_string(),
            Some(bus),
            None,
        );
        ui.apply_event(TurnEvent::Done {
            response: "ok".to_string(),
            events: vec![],
        });
        assert!(matches!(
            sub.try_recv(),
            Some(Event::AgentDone { success: true, .. })
        ));
        assert_eq!(ui.turns, 1);

        ui.apply_event(TurnEvent::Failed("boom".to_string()));
        assert!(matches!(sub.try_recv(), Some(Event::AgentFailed { .. })));
    }

    #[tokio::test]
    async fn status_line_surfaces_cached_proxy_spend() {
        let mut ui = test_ui();
        assert!(!ui.status_line().unwrap().contains('$'));
        ui.spend = Some((1234, 0.0567));
        let line = ui.status_line().unwrap();
        assert!(
            line.contains("1234 tok") && line.contains("$0.0567"),
            "{line}"
        );
    }

    #[test]
    fn sandbox_terminal_tool_runs_on_host_without_a_spec() {
        let tool = SandboxTerminalTool {
            spec: None,
            workdir: std::env::temp_dir(),
        };
        let out = tool
            .execute(r#"{"command": "echo hi"}"#, &mut ToolContext::default())
            .expect("command runs");
        assert!(out.output().contains("hi"));
        // Malformed args are a clean tool error, not a panic.
        assert!(tool.execute("{}", &mut ToolContext::default()).is_err());
    }
}
