//! The first-run onboarding wizard (`thegn setup`, roadmap AO 489).
//!
//! A multi-step modal — same interaction grammar as [`crate::env_wizard`] /
//! [`crate::terminal_wizard`] — that walks a new user through the setup
//! surface: paths, forge (GitHub) auth, issue trackers, remote hosts, sandbox
//! backend, appearance, and a coding agent, ending on a getting-started tour.
//! Every step is skippable; a step only writes config when the user changed a
//! value (skip = keep defaults). Re-runnable any time via the palette's
//! "Setup wizard…" or the `thegn setup` subcommand; steps pre-fill from the
//! effective config so a re-run edits rather than resets.
//!
//! Pure over its inputs: renders + handles keys on the event loop (zero idle
//! events). Everything with I/O — the gh/sandbox/ssh probes, config writes,
//! the `gh auth login` pane — is loop-side glue in
//! [`crate::handlers::onboarding`], fed back through the refresh channel
//! (`RefreshKind::Onboarding`) like every other off-thread producer.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use thegn_core::config::Config;

/// Requested by `thegn setup`: arm the wizard on compositor start even when
/// the first-run flag is already set. Written by `main()` before the TUI
/// launches, read once at startup.
static SETUP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn request_setup_on_start() {
    SETUP_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn setup_requested() -> bool {
    SETUP_REQUESTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The `ui_state` key marking the wizard as seen (scope `""`). Any value.
pub const UI_STATE_KEY: &str = "onboarding_done";

/// The wizard's steps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Paths,
    Forge,
    Issues,
    Hosts,
    Sandbox,
    Appearance,
    Agent,
    Tour,
}

const STEPS: &[Step] = &[
    Step::Welcome,
    Step::Paths,
    Step::Forge,
    Step::Issues,
    Step::Hosts,
    Step::Sandbox,
    Step::Appearance,
    Step::Agent,
    Step::Tour,
];

/// One focusable row inside a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    // Paths
    WorktreesDir,
    RepoRoots,
    // Forge
    ForgeAction,
    // Issues
    IssuesProvider,
    IssuesToken,
    LinearTeam,
    JiraUrl,
    JiraEmail,
    JiraProject,
    // Hosts
    HostName,
    HostSsh,
    // Sandbox
    SandboxBackend,
    SandboxProfile,
    // Appearance
    ThemePreset,
    KeymapPreset,
    // Agent
    AgentAction,
}

/// An off-thread probe the loop should run for the wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeRequest {
    /// `gh` install + auth state (`gh auth status`).
    Forge,
    /// PATH presence of each backend in the sandbox chain.
    Sandbox(Vec<String>),
    /// Reachability of a just-written ssh host (`user@box:port`).
    Host { name: String, ssh: String },
}

/// A probe's answer, delivered via `RefreshKind::Onboarding`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Forge(ForgeStatus),
    /// `(backend name, present on PATH)` per chain entry, chain order.
    Sandbox(Vec<(String, bool)>),
    Host {
        name: String,
        ok: bool,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeStatus {
    NotInstalled,
    NotAuthenticated,
    /// Authenticated; the string is the `gh auth status` login/host summary.
    Authenticated(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe<T> {
    Idle,
    Pending,
    Done(T),
}

/// A config mutation the loop applies when the user leaves a step forward.
/// Typed (not raw closures) so the state machine stays pure + unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// `config_write::set_key` on the global config.
    Set { key: String, value: String },
    /// `config_write::set_string_array` on the global config.
    SetArray { key: String, items: Vec<String> },
    /// Store `token` via the secret backend and point `key` at the ref.
    Secret {
        name: String,
        key: String,
        token: String,
    },
    /// `config_write::upsert_host`: `[host.<name>]` + `[host.<name>.ssh]`.
    Host { name: String, ssh: String },
    /// The keymap preset (persisted to `ui_state`, not the config file).
    KeymapPreset(String),
}

/// Side effects for the loop to perform. The wizard stays pure; the loop's
/// [`crate::handlers::onboarding::apply_outcome`] executes these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effects {
    pub writes: Vec<WriteOp>,
    pub probe: Option<ProbeRequest>,
    /// Spawn `gh auth login` in an interactive pane; re-probe on exit.
    pub login: bool,
    /// Spawn `thegn agent setup` in an interactive pane.
    pub agent_setup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    /// Perform side effects; the wizard stays open.
    Do(Effects),
    /// Close the wizard (`completed` = reached the end vs dismissed early).
    /// The final effects carry any last-step writes.
    Close {
        completed: bool,
        effects: Effects,
    },
}

const KEYMAP_PRESETS: &[&str] = &["default", "vscode", "jetbrains"];
const ISSUE_PROVIDERS: &[&str] = &["none", "github", "linear", "jira"];
const SANDBOX_PROFILES: &[&str] = &["hardened", "open", "sealed", "sealed-tunnel"];

pub struct OnboardingWizard {
    step: Step,
    focus: Field,
    // Effective-config snapshot the diff-on-leave compares against.
    init_worktrees_dir: String,
    init_repo_roots: String,
    init_theme: String,
    init_keymap: String,
    init_backend: String,
    init_profile: String,
    init_issues_provider: String,
    // Paths
    worktrees_dir: String,
    repo_roots: String,
    // Forge
    forge: Probe<ForgeStatus>,
    forge_action: usize,
    // Issues
    issues_sel: usize,
    issues_token: String,
    linear_team: String,
    jira_url: String,
    jira_email: String,
    jira_project: String,
    // Hosts
    host_name: String,
    host_ssh: String,
    // Sandbox
    sandbox_rows: Vec<String>,
    sandbox_avail: Probe<Vec<(String, bool)>>,
    sandbox_sel: usize,
    profile_sel: usize,
    // Appearance
    theme_sel: usize,
    keymap_sel: usize,
    // Agent
    agent_run: bool,
    keyring: bool,
    // Tour chord hints, resolved once from the live keymap.
    hint_new_worktree: String,
    hint_palette: String,
}

impl OnboardingWizard {
    pub fn new(cfg: &Config) -> Self {
        let repo_roots = cfg.repo_roots.join(", ");
        let backend = cfg.sandbox.backend.as_str().to_string();
        let profile = cfg.sandbox.profile.as_str().to_string();
        let theme = if cfg.theme.preset.is_empty() {
            "prism".to_string()
        } else {
            cfg.theme.preset.clone()
        };
        let keymap = if cfg.keymap_preset.is_empty() {
            "default".to_string()
        } else {
            cfg.keymap_preset.clone()
        };
        let issues_provider = cfg
            .issues
            .active_providers()
            .first()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "none".to_string());
        // Cycle rows: "auto" + the effective chain (a concrete configured
        // backend collapses the chain to itself, mirroring doctor's view).
        let mut sandbox_rows = vec!["auto".to_string()];
        if backend == "auto" {
            sandbox_rows.extend(cfg.sandbox.backend_chain.iter().cloned());
        } else {
            sandbox_rows.push(backend.clone());
            sandbox_rows.extend(
                cfg.sandbox
                    .backend_chain
                    .iter()
                    .filter(|b| **b != backend)
                    .cloned(),
            );
        }
        let pos = |list: &[&str], v: &str| list.iter().position(|x| *x == v).unwrap_or(0);
        OnboardingWizard {
            step: Step::Welcome,
            focus: Field::ForgeAction, // unused until a fielded step; reset on entry
            init_worktrees_dir: cfg.worktrees_dir.clone(),
            init_repo_roots: repo_roots.clone(),
            init_theme: theme.clone(),
            init_keymap: keymap.clone(),
            init_backend: backend.clone(),
            init_profile: profile.clone(),
            init_issues_provider: issues_provider.clone(),
            worktrees_dir: cfg.worktrees_dir.clone(),
            repo_roots,
            forge: Probe::Idle,
            forge_action: 0,
            issues_sel: pos(ISSUE_PROVIDERS, &issues_provider),
            issues_token: String::new(),
            linear_team: String::new(),
            jira_url: String::new(),
            jira_email: String::new(),
            jira_project: String::new(),
            host_name: String::new(),
            host_ssh: String::new(),
            sandbox_sel: sandbox_rows.iter().position(|b| *b == backend).unwrap_or(0),
            sandbox_rows,
            sandbox_avail: Probe::Idle,
            profile_sel: pos(SANDBOX_PROFILES, &profile),
            theme_sel: thegn_core::theme::PRESETS
                .iter()
                .position(|p| *p == theme)
                .unwrap_or(0),
            keymap_sel: pos(KEYMAP_PRESETS, &keymap),
            agent_run: false,
            keyring: crate::secret::keyring_available(),
            hint_new_worktree: crate::keymap::chord_hint_for(cfg, "new-worktree")
                .unwrap_or_else(|| "alt-w".to_string()),
            hint_palette: crate::keymap::chord_hint_for(cfg, "palette")
                .unwrap_or_else(|| "alt-k".to_string()),
        }
    }

    /// Deliver an off-thread probe answer into the live wizard.
    pub fn apply_probe(&mut self, result: ProbeResult) {
        match result {
            ProbeResult::Forge(s) => {
                self.forge_action = 0;
                self.forge = Probe::Done(s);
            }
            ProbeResult::Sandbox(rows) => self.sandbox_avail = Probe::Done(rows),
            // Host reachability lands in the status bar (the step has advanced).
            ProbeResult::Host { .. } => {}
        }
    }

    /// The `gh auth login` pane closed — show "checking…" until the re-probe lands.
    pub fn forge_recheck(&mut self) {
        self.forge = Probe::Pending;
    }

    // ---- step flow -------------------------------------------------------

    fn step_index(&self) -> usize {
        STEPS.iter().position(|s| *s == self.step).unwrap_or(0)
    }

    /// The focusable fields of the current step, top to bottom.
    fn fields(&self) -> Vec<Field> {
        match self.step {
            Step::Welcome | Step::Tour => vec![],
            Step::Paths => vec![Field::WorktreesDir, Field::RepoRoots],
            Step::Forge => vec![Field::ForgeAction],
            Step::Issues => {
                let mut f = vec![Field::IssuesProvider];
                match self.issues_provider() {
                    "linear" => f.extend([Field::IssuesToken, Field::LinearTeam]),
                    "jira" => f.extend([
                        Field::IssuesToken,
                        Field::JiraUrl,
                        Field::JiraEmail,
                        Field::JiraProject,
                    ]),
                    _ => {}
                }
                f
            }
            Step::Hosts => vec![Field::HostName, Field::HostSsh],
            Step::Sandbox => vec![Field::SandboxBackend, Field::SandboxProfile],
            Step::Appearance => vec![Field::ThemePreset, Field::KeymapPreset],
            Step::Agent => vec![Field::AgentAction],
        }
    }

    /// Enter `step`: reset focus and return the probe it needs (if unprobed).
    fn enter(&mut self, step: Step) -> Option<ProbeRequest> {
        self.step = step;
        if let Some(first) = self.fields().first() {
            self.focus = *first;
        }
        match step {
            Step::Forge if self.forge == Probe::Idle => {
                self.forge = Probe::Pending;
                Some(ProbeRequest::Forge)
            }
            Step::Sandbox if self.sandbox_avail == Probe::Idle => {
                self.sandbox_avail = Probe::Pending;
                let chain: Vec<String> = self
                    .sandbox_rows
                    .iter()
                    .filter(|b| *b != "auto")
                    .cloned()
                    .collect();
                Some(ProbeRequest::Sandbox(chain))
            }
            _ => None,
        }
    }

    /// Leave the current step forward: collect its config writes.
    fn leave_writes(&self) -> Vec<WriteOp> {
        let mut w = Vec::new();
        let changed = |a: &str, b: &str| a.trim() != b.trim();
        match self.step {
            Step::Paths => {
                if changed(&self.worktrees_dir, &self.init_worktrees_dir)
                    && !self.worktrees_dir.trim().is_empty()
                {
                    w.push(WriteOp::Set {
                        key: "worktrees_dir".into(),
                        value: self.worktrees_dir.trim().to_string(),
                    });
                }
                if changed(&self.repo_roots, &self.init_repo_roots) {
                    let items: Vec<String> = self
                        .repo_roots
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    w.push(WriteOp::SetArray {
                        key: "repo_roots".into(),
                        items,
                    });
                }
            }
            Step::Issues => {
                let provider = self.issues_provider();
                if changed(provider, &self.init_issues_provider) {
                    w.push(WriteOp::Set {
                        key: "issues.provider".into(),
                        value: provider.to_string(),
                    });
                }
                let token = self.issues_token.trim();
                match provider {
                    "linear" => {
                        if !token.is_empty() {
                            w.push(WriteOp::Secret {
                                name: "issues-linear".into(),
                                key: "issues.linear.api_key".into(),
                                token: token.to_string(),
                            });
                        }
                        if !self.linear_team.trim().is_empty() {
                            w.push(WriteOp::Set {
                                key: "issues.linear.team_id".into(),
                                value: self.linear_team.trim().to_string(),
                            });
                        }
                    }
                    "jira" => {
                        if !token.is_empty() {
                            w.push(WriteOp::Secret {
                                name: "issues-jira".into(),
                                key: "issues.jira.api_token".into(),
                                token: token.to_string(),
                            });
                        }
                        for (key, val) in [
                            ("issues.jira.base_url", &self.jira_url),
                            ("issues.jira.email", &self.jira_email),
                            ("issues.jira.project_key", &self.jira_project),
                        ] {
                            if !val.trim().is_empty() {
                                w.push(WriteOp::Set {
                                    key: key.into(),
                                    value: val.trim().to_string(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            Step::Hosts => {
                let (name, ssh) = (self.host_name.trim(), self.host_ssh.trim());
                if !name.is_empty() && !ssh.is_empty() {
                    w.push(WriteOp::Host {
                        name: name.replace(' ', "-"),
                        ssh: ssh.to_string(),
                    });
                }
            }
            Step::Sandbox => {
                let backend = self.sandbox_backend();
                if changed(backend, &self.init_backend) {
                    w.push(WriteOp::Set {
                        key: "sandbox.backend".into(),
                        value: backend.to_string(),
                    });
                }
                let profile = SANDBOX_PROFILES[self.profile_sel];
                if changed(profile, &self.init_profile) {
                    w.push(WriteOp::Set {
                        key: "sandbox.profile".into(),
                        value: profile.to_string(),
                    });
                }
            }
            Step::Appearance => {
                let theme = thegn_core::theme::PRESETS[self.theme_sel];
                if changed(theme, &self.init_theme) {
                    w.push(WriteOp::Set {
                        key: "theme.preset".into(),
                        value: theme.to_string(),
                    });
                }
                let keymap = KEYMAP_PRESETS[self.keymap_sel];
                if changed(keymap, &self.init_keymap) {
                    w.push(WriteOp::KeymapPreset(keymap.to_string()));
                }
            }
            _ => {}
        }
        w
    }

    /// Advance past the current step (writes + next step's probe), or finish.
    fn advance(&mut self) -> Outcome {
        let mut effects = Effects {
            writes: self.leave_writes(),
            ..Effects::default()
        };
        // A just-written host gets a reachability probe (result → status bar).
        if self.step == Step::Hosts
            && let Some(WriteOp::Host { name, ssh }) = effects
                .writes
                .iter()
                .find(|w| matches!(w, WriteOp::Host { .. }))
        {
            effects.probe = Some(ProbeRequest::Host {
                name: name.clone(),
                ssh: ssh.clone(),
            });
        }
        let i = self.step_index();
        match STEPS.get(i + 1) {
            Some(next) => {
                let probe = self.enter(*next);
                // enter() only probes Forge/Sandbox; a Hosts leave-probe (set
                // above) and an enter-probe can't both occur on one advance.
                if effects.probe.is_none() {
                    effects.probe = probe;
                }
                Outcome::Do(effects)
            }
            None => Outcome::Close {
                completed: true,
                effects,
            },
        }
    }

    fn back(&mut self) -> Outcome {
        let i = self.step_index();
        match i.checked_sub(1).and_then(|p| STEPS.get(p)) {
            Some(prev) => {
                // Going back never writes; re-entering re-probes only if Idle.
                let probe = self.enter(*prev);
                Outcome::Do(Effects {
                    probe,
                    ..Effects::default()
                })
            }
            None => Outcome::Close {
                completed: false,
                effects: Effects::default(),
            },
        }
    }

    // ---- current selections ----------------------------------------------

    fn issues_provider(&self) -> &'static str {
        ISSUE_PROVIDERS[self.issues_sel.min(ISSUE_PROVIDERS.len() - 1)]
    }

    fn sandbox_backend(&self) -> &str {
        self.sandbox_rows
            .get(self.sandbox_sel)
            .map(String::as_str)
            .unwrap_or("auto")
    }

    /// Forge-step action rows for the current probe state.
    fn forge_actions(&self) -> Vec<&'static str> {
        match &self.forge {
            Probe::Done(ForgeStatus::NotAuthenticated) => {
                vec!["login now", "re-check", "continue"]
            }
            Probe::Done(ForgeStatus::NotInstalled) => vec!["re-check", "continue"],
            Probe::Done(ForgeStatus::Authenticated(_)) => vec!["continue", "re-check"],
            _ => vec!["continue"],
        }
    }

    // ---- input -------------------------------------------------------------

    fn text_mut(&mut self, f: Field) -> Option<&mut String> {
        match f {
            Field::WorktreesDir => Some(&mut self.worktrees_dir),
            Field::RepoRoots => Some(&mut self.repo_roots),
            Field::IssuesToken => Some(&mut self.issues_token),
            Field::LinearTeam => Some(&mut self.linear_team),
            Field::JiraUrl => Some(&mut self.jira_url),
            Field::JiraEmail => Some(&mut self.jira_email),
            Field::JiraProject => Some(&mut self.jira_project),
            Field::HostName => Some(&mut self.host_name),
            Field::HostSsh => Some(&mut self.host_ssh),
            _ => None,
        }
    }

    fn is_cycle(f: Field) -> bool {
        matches!(
            f,
            Field::ForgeAction
                | Field::IssuesProvider
                | Field::SandboxBackend
                | Field::SandboxProfile
                | Field::ThemePreset
                | Field::KeymapPreset
                | Field::AgentAction
        )
    }

    fn cycle(&mut self, delta: i32) {
        let wrap = |sel: usize, n: usize| -> usize {
            let n = n as i32;
            (((sel as i32 + delta) % n + n) % n) as usize
        };
        match self.focus {
            Field::ForgeAction => {
                self.forge_action = wrap(self.forge_action, self.forge_actions().len())
            }
            Field::IssuesProvider => self.issues_sel = wrap(self.issues_sel, ISSUE_PROVIDERS.len()),
            Field::SandboxBackend => {
                self.sandbox_sel = wrap(self.sandbox_sel, self.sandbox_rows.len())
            }
            Field::SandboxProfile => {
                self.profile_sel = wrap(self.profile_sel, SANDBOX_PROFILES.len())
            }
            Field::ThemePreset => {
                self.theme_sel = wrap(self.theme_sel, thegn_core::theme::PRESETS.len())
            }
            Field::KeymapPreset => self.keymap_sel = wrap(self.keymap_sel, KEYMAP_PRESETS.len()),
            Field::AgentAction => self.agent_run = !self.agent_run,
            _ => {}
        }
    }

    fn move_focus(&mut self, delta: i32) {
        let fields = self.fields();
        if fields.is_empty() {
            return;
        }
        let cur = fields.iter().position(|&f| f == self.focus).unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, fields.len() as i32 - 1) as usize;
        self.focus = fields[next];
    }

    /// Inject bracketed-paste text into the focused text field (tokens/paths).
    pub fn handle_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        let f = self.focus;
        if let Some(s) = self.text_mut(f) {
            s.push_str(&clean);
        }
    }

    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> Outcome {
        if mods.contains(Modifiers::CTRL) {
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => Outcome::Close {
                    completed: false,
                    effects: Effects::default(),
                },
                _ => Outcome::Pending,
            };
        }
        if mods.contains(Modifiers::ALT) || mods.contains(Modifiers::SUPER) {
            return Outcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return self.back();
        }
        let fields = self.fields();
        let on_last = fields.last() == Some(&self.focus) || fields.is_empty();
        match key {
            KeyCode::UpArrow => self.move_focus(-1),
            KeyCode::DownArrow => self.move_focus(1),
            KeyCode::LeftArrow if Self::is_cycle(self.focus) => self.cycle(-1),
            KeyCode::RightArrow if Self::is_cycle(self.focus) => self.cycle(1),
            KeyCode::Enter => {
                // Forge/Agent action rows dispatch on enter.
                if self.step == Step::Forge {
                    match self.forge_actions().get(self.forge_action).copied() {
                        Some("login now") => {
                            self.forge = Probe::Pending;
                            return Outcome::Do(Effects {
                                login: true,
                                ..Effects::default()
                            });
                        }
                        Some("re-check") => {
                            self.forge = Probe::Pending;
                            self.forge_action = 0;
                            return Outcome::Do(Effects {
                                probe: Some(ProbeRequest::Forge),
                                ..Effects::default()
                            });
                        }
                        _ => return self.advance(),
                    }
                }
                if self.step == Step::Agent && self.focus == Field::AgentAction {
                    // enter on the action row runs setup AND advances; plain
                    // enter with "skip" selected just advances.
                    if self.agent_run_selected() {
                        let mut out = self.advance();
                        if let Outcome::Do(e) | Outcome::Close { effects: e, .. } = &mut out {
                            e.agent_setup = true;
                        }
                        return out;
                    }
                    return self.advance();
                }
                if on_last {
                    return self.advance();
                }
                self.move_focus(1);
            }
            KeyCode::Backspace => {
                let f = self.focus;
                if let Some(s) = self.text_mut(f) {
                    s.pop();
                }
            }
            KeyCode::Char(c) => {
                let c = *c;
                let f = self.focus;
                if let Some(s) = self.text_mut(f) {
                    s.push(c);
                }
            }
            _ => {}
        }
        Outcome::Pending
    }

    fn agent_run_selected(&self) -> bool {
        self.agent_run
    }

    // ---- render ------------------------------------------------------------

    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let lines = self.body_lines();
        let spec = LayerSpec {
            title: format!("setup · {}", self.title()),
            badge: Some(format!(" {}/{} ", self.step_index() + 1, STEPS.len())),
            cols: 72,
            rows: lines.len() + 2,
            anchor: Anchor::Center,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        for (i, line) in lines.iter().enumerate() {
            seg::draw_line(surface, inner.x, inner.y + i, inner.cols, line, panel);
        }
        let fields = self.fields();
        let on_last = fields.last() == Some(&self.focus) || fields.is_empty();
        let enter = if self.step == Step::Tour {
            "enter finish"
        } else if on_last {
            "enter next step"
        } else {
            "enter next"
        };
        let esc = if self.step == Step::Welcome {
            "esc later"
        } else {
            "esc back"
        };
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows - 1,
            inner.cols,
            &Line::segs(vec![seg(
                Tok::Slot(S::Faint),
                format!("↑↓ move · ←→ change · {enter} · {esc}"),
            )]),
            panel,
        );
    }

    fn title(&self) -> &'static str {
        match self.step {
            Step::Welcome => "welcome",
            Step::Paths => "paths",
            Step::Forge => "forge (github)",
            Step::Issues => "issue tracker",
            Step::Hosts => "remote hosts",
            Step::Sandbox => "sandbox",
            Step::Appearance => "appearance",
            Step::Agent => "coding agent",
            Step::Tour => "you're set",
        }
    }

    fn body_lines(&self) -> Vec<Line> {
        let note = |t: &str| Line::segs(vec![seg(Tok::Slot(S::Faint), t.to_string())]);
        let blank = || Line::segs(vec![sp(0)]);
        match self.step {
            Step::Welcome => vec![
                Line::segs(vec![
                    seg(Tok::Slot(S::Text), "Welcome to ".to_string()),
                    seg(Tok::Slot(S::Accent), "thegn".to_string()).bold(),
                    seg(
                        Tok::Slot(S::Text),
                        " — every git worktree a tab, everything instant.".to_string(),
                    ),
                ]),
                blank(),
                note("This wizard sets up forge auth, issue trackers, remote hosts,"),
                note("sandboxing, and appearance. Every step is optional — nothing is"),
                note("written unless you change it."),
                blank(),
                note("Re-run any time: `thegn setup`, or \"Setup wizard…\" in the palette."),
            ],
            Step::Paths => vec![
                self.text_row(
                    Field::WorktreesDir,
                    "worktrees ",
                    &self.worktrees_dir,
                    "directory for managed worktrees",
                    false,
                ),
                self.text_row(
                    Field::RepoRoots,
                    "repo roots",
                    &self.repo_roots,
                    "comma-separated dirs scanned for repos (e.g. ~/code)",
                    false,
                ),
                blank(),
                note("repo roots power repo discovery in the workspace picker."),
            ],
            Step::Forge => {
                let status = match &self.forge {
                    Probe::Idle | Probe::Pending => {
                        vec![seg(Tok::Slot(S::Faint), "⋯ checking gh…".to_string())]
                    }
                    Probe::Done(ForgeStatus::NotInstalled) => vec![
                        seg(Tok::Hue(thegn_core::theme::Hue::Amber), "✗ ".to_string()),
                        seg(
                            Tok::Slot(S::Text),
                            "gh not installed — https://cli.github.com".to_string(),
                        ),
                    ],
                    Probe::Done(ForgeStatus::NotAuthenticated) => vec![
                        seg(Tok::Hue(thegn_core::theme::Hue::Amber), "✗ ".to_string()),
                        seg(
                            Tok::Slot(S::Text),
                            "gh installed, but not authenticated".to_string(),
                        ),
                    ],
                    Probe::Done(ForgeStatus::Authenticated(who)) => vec![
                        seg(Tok::Hue(thegn_core::theme::Hue::Green), "● ".to_string()),
                        seg(Tok::Slot(S::Text), format!("authenticated — {who}")),
                    ],
                };
                let action = self
                    .forge_actions()
                    .get(self.forge_action)
                    .copied()
                    .unwrap_or("continue");
                vec![
                    Line::segs(status),
                    blank(),
                    Line::segs(self.cycle_row(
                        "action    ",
                        self.focus == Field::ForgeAction,
                        action,
                    )),
                    blank(),
                    note("PR panel, checks, and reviews ride the gh CLI's auth."),
                ]
            }
            Step::Issues => {
                let mut lines = vec![Line::segs(self.cycle_row(
                    "provider  ",
                    self.focus == Field::IssuesProvider,
                    self.issues_provider(),
                ))];
                match self.issues_provider() {
                    "linear" => {
                        lines.push(self.text_row(
                            Field::IssuesToken,
                            "api key   ",
                            &self.issues_token,
                            "paste Linear API key (stored securely)",
                            true,
                        ));
                        lines.push(self.text_row(
                            Field::LinearTeam,
                            "team id   ",
                            &self.linear_team,
                            "· all teams",
                            false,
                        ));
                    }
                    "jira" => {
                        lines.push(self.text_row(
                            Field::IssuesToken,
                            "api token ",
                            &self.issues_token,
                            "paste Jira API token (stored securely)",
                            true,
                        ));
                        lines.push(self.text_row(
                            Field::JiraUrl,
                            "base url  ",
                            &self.jira_url,
                            "https://you.atlassian.net",
                            false,
                        ));
                        lines.push(self.text_row(
                            Field::JiraEmail,
                            "email     ",
                            &self.jira_email,
                            "account email",
                            false,
                        ));
                        lines.push(self.text_row(
                            Field::JiraProject,
                            "project   ",
                            &self.jira_project,
                            "· all projects",
                            false,
                        ));
                    }
                    "github" => lines.push(note("github issues auto-scope to each repo's remote.")),
                    _ => {}
                }
                lines.push(blank());
                let store = if self.keyring {
                    "OS keyring"
                } else {
                    "0600 file"
                };
                lines.push(note(&format!(
                    "issue feed is coming soon — config is saved now (tokens → {store})."
                )));
                lines
            }
            Step::Hosts => vec![
                self.text_row(
                    Field::HostName,
                    "name      ",
                    &self.host_name,
                    "· skip — no remote host",
                    false,
                ),
                self.text_row(
                    Field::HostSsh,
                    "ssh       ",
                    &self.host_ssh,
                    "user@box:port",
                    false,
                ),
                blank(),
                note("registers [host.<name>] — worktrees can then run on it."),
                note("leave empty to skip; add more later with `thegn host`."),
            ],
            Step::Sandbox => {
                let detected = match &self.sandbox_avail {
                    Probe::Idle | Probe::Pending => {
                        vec![seg(
                            Tok::Slot(S::Faint),
                            "⋯ probing container runtimes…".to_string(),
                        )]
                    }
                    Probe::Done(rows) => {
                        let mut segs = vec![seg(Tok::Slot(S::Faint), "detected  ".to_string())];
                        for (i, (name, ok)) in rows.iter().enumerate() {
                            if i > 0 {
                                segs.push(seg(Tok::Slot(S::Faint), " · ".to_string()));
                            }
                            segs.push(if *ok {
                                seg(Tok::Hue(thegn_core::theme::Hue::Green), format!("{name} ●"))
                            } else {
                                seg(Tok::Slot(S::Faint), format!("{name} ✗"))
                            });
                        }
                        segs
                    }
                };
                vec![
                    Line::segs(detected),
                    blank(),
                    Line::segs(self.cycle_row(
                        "backend   ",
                        self.focus == Field::SandboxBackend,
                        self.sandbox_backend(),
                    )),
                    Line::segs(self.cycle_row(
                        "profile   ",
                        self.focus == Field::SandboxProfile,
                        SANDBOX_PROFILES[self.profile_sel],
                    )),
                    blank(),
                    note("auto walks the chain and picks the first available backend."),
                ]
            }
            Step::Appearance => vec![
                Line::segs(self.cycle_row(
                    "theme     ",
                    self.focus == Field::ThemePreset,
                    thegn_core::theme::PRESETS[self.theme_sel],
                )),
                Line::segs(self.cycle_row(
                    "keymap    ",
                    self.focus == Field::KeymapPreset,
                    KEYMAP_PRESETS[self.keymap_sel],
                )),
                blank(),
                note("vscode/jetbrains layer familiar chords over the defaults."),
            ],
            Step::Agent => vec![
                Line::segs(self.cycle_row(
                    "agent     ",
                    self.focus == Field::AgentAction,
                    if self.agent_run {
                        "run `thegn agent setup` now"
                    } else {
                        "skip"
                    },
                )),
                blank(),
                note("installs + configures the managed coding agent (pi)."),
                note("also available later: `thegn agent setup`."),
            ],
            Step::Tour => vec![
                Line::segs(vec![
                    seg(Tok::Hue(thegn_core::theme::Hue::Green), "● ".to_string()),
                    seg(Tok::Slot(S::Text), "setup saved — a quick map:".to_string()),
                ]),
                blank(),
                self.hint_line(&self.hint_new_worktree.clone(), "new worktree (a tab)"),
                self.hint_line(&self.hint_palette.clone(), "command palette — every action"),
                self.hint_line("?", "keybind help (in the sidebar)"),
                self.hint_line("thegn doctor", "capability + sandbox report"),
                self.hint_line("thegn setup", "re-run this wizard"),
            ],
        }
    }

    fn hint_line(&self, chord: &str, what: &str) -> Line {
        Line::segs(vec![
            sp(2),
            seg(Tok::Slot(S::Accent), format!("{chord:<14}")).bold(),
            seg(Tok::Slot(S::Text), what.to_string()),
        ])
    }

    fn text_row(&self, f: Field, label: &str, val: &str, placeholder: &str, mask: bool) -> Line {
        let focused = self.focus == f;
        let label_fg = if focused {
            Tok::Slot(S::Accent)
        } else {
            Tok::Slot(S::Faint)
        };
        let shown = if val.is_empty() {
            seg(Tok::Slot(S::Faint), placeholder.to_string())
        } else if mask {
            seg(Tok::Slot(S::Text), "•".repeat(val.chars().count()))
        } else {
            seg(Tok::Slot(S::Text), val.to_string())
        };
        Line::segs(vec![
            seg(label_fg, format!("{label}❯ ")).bold(),
            shown,
            if focused {
                seg(Tok::Slot(S::Accent), "▏".to_string())
            } else {
                sp(0)
            },
        ])
    }

    fn cycle_row(&self, label: &str, focused: bool, value: &str) -> Vec<Seg> {
        let fg = if focused {
            Tok::Slot(S::Accent)
        } else {
            Tok::Slot(S::Faint)
        };
        let mut segs = vec![seg(fg, label.to_string()).bold()];
        if focused {
            segs.push(seg(Tok::Slot(S::Accent), "‹ ".to_string()));
            segs.push(seg(Tok::Slot(S::Text), value.to_string()).bold());
            segs.push(seg(Tok::Slot(S::Accent), " ›".to_string()));
        } else {
            segs.push(seg(Tok::Slot(S::Text), value.to_string()));
        }
        segs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    fn wiz() -> OnboardingWizard {
        OnboardingWizard::new(&Config::default())
    }

    fn typ(w: &mut OnboardingWizard, s: &str) {
        for c in s.chars() {
            w.handle_key(&KeyCode::Char(c), NONE);
        }
    }

    fn enter(w: &mut OnboardingWizard) -> Outcome {
        w.handle_key(&KeyCode::Enter, NONE)
    }

    /// Walk forward from Welcome to `target` with plain Enters, collecting the
    /// outcomes (each fielded step advances from its last field).
    fn goto(w: &mut OnboardingWizard, target: Step) {
        for _ in 0..64 {
            if w.step == target {
                return;
            }
            let fields = w.fields();
            let on_last = fields.last() == Some(&w.focus) || fields.is_empty();
            if !on_last {
                w.move_focus(1);
                continue;
            }
            enter(w);
        }
        panic!("never reached {target:?}");
    }

    #[test]
    fn welcome_enter_advances_and_esc_closes_uncompleted() {
        let mut w = wiz();
        assert_eq!(w.step, Step::Welcome);
        assert!(matches!(enter(&mut w), Outcome::Do(_)));
        assert_eq!(w.step, Step::Paths);
        let mut w2 = wiz();
        match w2.handle_key(&KeyCode::Escape, NONE) {
            Outcome::Close { completed, effects } => {
                assert!(!completed);
                assert!(effects.writes.is_empty());
            }
            o => panic!("expected close, got {o:?}"),
        }
    }

    #[test]
    fn ctrl_c_closes_anywhere() {
        let mut w = wiz();
        goto(&mut w, Step::Sandbox);
        assert!(matches!(
            w.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            Outcome::Close {
                completed: false,
                ..
            }
        ));
    }

    #[test]
    fn unchanged_steps_write_nothing() {
        let mut w = wiz();
        goto(&mut w, Step::Tour);
        match enter(&mut w) {
            Outcome::Close { completed, effects } => {
                assert!(completed);
                assert!(
                    effects.writes.is_empty(),
                    "skip-through must not write: {:?}",
                    effects.writes
                );
            }
            o => panic!("expected close, got {o:?}"),
        }
    }

    #[test]
    fn paths_write_only_changed_values() {
        let mut w = wiz();
        goto(&mut w, Step::Paths);
        assert_eq!(w.focus, Field::WorktreesDir);
        typ(&mut w, "x");
        enter(&mut w); // → RepoRoots
        typ(&mut w, "~/code, ~/oss");
        match enter(&mut w) {
            Outcome::Do(e) => {
                assert!(e.writes.iter().any(|op| matches!(
                    op,
                    WriteOp::Set { key, .. } if key == "worktrees_dir"
                )));
                assert!(e.writes.iter().any(|op| matches!(
                    op,
                    WriteOp::SetArray { key, items } if key == "repo_roots" && items.len() == 2
                )));
            }
            o => panic!("expected Do, got {o:?}"),
        }
        assert_eq!(w.step, Step::Forge);
    }

    #[test]
    fn entering_forge_and_sandbox_requests_probes_once() {
        let mut w = wiz();
        goto(&mut w, Step::Paths);
        w.move_focus(1);
        match enter(&mut w) {
            Outcome::Do(e) => assert_eq!(e.probe, Some(ProbeRequest::Forge)),
            o => panic!("expected Do, got {o:?}"),
        }
        assert_eq!(w.forge, Probe::Pending);
        // Back and forward again: no second probe while pending/done.
        w.handle_key(&KeyCode::Escape, NONE);
        assert_eq!(w.step, Step::Paths);
        w.move_focus(1); // re-entering a step resets focus to its first field
        match enter(&mut w) {
            Outcome::Do(e) => assert_eq!(e.probe, None),
            o => panic!("expected Do, got {o:?}"),
        }
        goto(&mut w, Step::Sandbox);
        assert_eq!(w.sandbox_avail, Probe::Pending);
    }

    #[test]
    fn forge_login_and_recheck_actions() {
        let mut w = wiz();
        goto(&mut w, Step::Forge);
        w.apply_probe(ProbeResult::Forge(ForgeStatus::NotAuthenticated));
        assert_eq!(w.forge_actions(), vec!["login now", "re-check", "continue"]);
        match enter(&mut w) {
            Outcome::Do(e) => assert!(e.login),
            o => panic!("expected login effect, got {o:?}"),
        }
        assert_eq!(w.step, Step::Forge, "login keeps the step open");
        // The login pane exited → recheck → authenticated → continue advances.
        w.forge_recheck();
        w.apply_probe(ProbeResult::Forge(ForgeStatus::Authenticated(
            "me".to_string(),
        )));
        assert_eq!(w.forge_actions()[0], "continue");
        assert!(matches!(enter(&mut w), Outcome::Do(_)));
        assert_eq!(w.step, Step::Issues);
    }

    #[test]
    fn issues_provider_gates_fields_and_stores_token_as_secret() {
        let mut w = wiz();
        goto(&mut w, Step::Issues);
        assert_eq!(w.fields(), vec![Field::IssuesProvider]);
        // cycle none → github → linear
        w.handle_key(&KeyCode::RightArrow, NONE);
        w.handle_key(&KeyCode::RightArrow, NONE);
        assert_eq!(w.issues_provider(), "linear");
        assert_eq!(
            w.fields(),
            vec![Field::IssuesProvider, Field::IssuesToken, Field::LinearTeam]
        );
        enter(&mut w);
        typ(&mut w, "lin_api_SECRET");
        enter(&mut w); // → team id
        match enter(&mut w) {
            Outcome::Do(e) => {
                assert!(e.writes.contains(&WriteOp::Set {
                    key: "issues.provider".into(),
                    value: "linear".into()
                }));
                assert!(e.writes.iter().any(|op| matches!(
                    op,
                    WriteOp::Secret { key, token, .. }
                        if key == "issues.linear.api_key" && token == "lin_api_SECRET"
                )));
            }
            o => panic!("expected Do, got {o:?}"),
        }
    }

    #[test]
    fn hosts_filled_writes_and_probes_empty_skips() {
        let mut w = wiz();
        goto(&mut w, Step::Hosts);
        typ(&mut w, "build box");
        enter(&mut w);
        typ(&mut w, "me@build.example.com:2222");
        match enter(&mut w) {
            Outcome::Do(e) => {
                assert!(e.writes.contains(&WriteOp::Host {
                    name: "build-box".into(),
                    ssh: "me@build.example.com:2222".into()
                }));
                assert!(matches!(e.probe, Some(ProbeRequest::Host { .. })));
            }
            o => panic!("expected Do, got {o:?}"),
        }
        // Empty fields skip cleanly.
        let mut w2 = wiz();
        goto(&mut w2, Step::Hosts);
        w2.move_focus(1);
        match enter(&mut w2) {
            Outcome::Do(e) => {
                assert!(e.writes.is_empty());
                // Sandbox enter-probe replaces the absent host probe.
                assert!(matches!(e.probe, Some(ProbeRequest::Sandbox(_))));
            }
            o => panic!("expected Do, got {o:?}"),
        }
    }

    #[test]
    fn sandbox_selection_writes_backend_and_profile() {
        let mut w = wiz();
        goto(&mut w, Step::Sandbox);
        w.apply_probe(ProbeResult::Sandbox(vec![("podman-rootless".into(), true)]));
        w.handle_key(&KeyCode::RightArrow, NONE); // auto → first chain entry
        let picked = w.sandbox_backend().to_string();
        w.move_focus(1);
        w.handle_key(&KeyCode::RightArrow, NONE); // hardened → open
        match enter(&mut w) {
            Outcome::Do(e) => {
                assert!(e.writes.contains(&WriteOp::Set {
                    key: "sandbox.backend".into(),
                    value: picked
                }));
                assert!(e.writes.contains(&WriteOp::Set {
                    key: "sandbox.profile".into(),
                    value: "open".into()
                }));
            }
            o => panic!("expected Do, got {o:?}"),
        }
    }

    #[test]
    fn appearance_writes_theme_key_and_keymap_preset_op() {
        let mut w = wiz();
        goto(&mut w, Step::Appearance);
        w.handle_key(&KeyCode::RightArrow, NONE); // prism → storm
        w.move_focus(1);
        w.handle_key(&KeyCode::RightArrow, NONE); // default → vscode
        match enter(&mut w) {
            Outcome::Do(e) => {
                assert!(e.writes.contains(&WriteOp::Set {
                    key: "theme.preset".into(),
                    value: "storm".into()
                }));
                assert!(
                    e.writes
                        .contains(&WriteOp::KeymapPreset("vscode".to_string()))
                );
            }
            o => panic!("expected Do, got {o:?}"),
        }
    }

    #[test]
    fn agent_toggle_requests_setup_spawn() {
        let mut w = wiz();
        goto(&mut w, Step::Agent);
        w.handle_key(&KeyCode::RightArrow, NONE); // skip → run
        match enter(&mut w) {
            Outcome::Do(e) => assert!(e.agent_setup),
            o => panic!("expected Do, got {o:?}"),
        }
        assert_eq!(w.step, Step::Tour);
    }

    #[test]
    fn rerun_prefills_from_effective_config() {
        let mut cfg = Config::default();
        cfg.theme.preset = "storm".into();
        cfg.keymap_preset = "vscode".into();
        cfg.repo_roots = vec!["~/code".into()];
        let w = OnboardingWizard::new(&cfg);
        assert_eq!(thegn_core::theme::PRESETS[w.theme_sel], "storm");
        assert_eq!(KEYMAP_PRESETS[w.keymap_sel], "vscode");
        assert_eq!(w.repo_roots, "~/code");
        // Pre-filled values are the baseline: advancing writes nothing.
        let mut w = OnboardingWizard::new(&cfg);
        goto(&mut w, Step::Tour);
        match enter(&mut w) {
            Outcome::Close { effects, .. } => assert!(effects.writes.is_empty()),
            o => panic!("expected close, got {o:?}"),
        }
    }

    #[test]
    fn paste_lands_in_focused_text_field() {
        let mut w = wiz();
        goto(&mut w, Step::Hosts);
        w.handle_paste("me@box:22\n");
        assert_eq!(w.host_name, "me@box:22", "control chars stripped");
    }
}
