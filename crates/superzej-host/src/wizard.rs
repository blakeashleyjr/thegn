//! The new-worktree wizard (Alt+w) and its creation pipeline.
//!
//! The wizard collects every choice up front on one plane — branch name
//! (prefilled with a pregenerated adjective-noun candidate), host (execution
//! environment), sandbox backend, program — while a
//! background worker speculatively creates the worktree under the candidate
//! name the moment the wizard opens, so the dominant cost (`git worktree
//! add`'s full checkout) overlaps with the user reading the form. Accepting
//! the prefill costs nothing extra; a custom name is a metadata-only
//! `git branch -m` + `git worktree move`; cancelling removes the speculative
//! worktree.
//!
//! Split per the event-model invariant: the wizard/progress state machines
//! and rendering live on the loop; [`run_worker`] owns every blocking step
//! (git, DB, sandbox ensure) on a `spawn_blocking` thread, receives wizard
//! decisions over a [`WizardCmd`] channel, and reports [`CreateEvent`]s back
//! over a tokio mpsc + `TerminalWaker` pulse. The spinner is driven by a
//! short-lived ticker thread alive only while the progress overlay is shown —
//! idle superzej still produces zero events.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::remote::GitLoc;
use superzej_core::theme::Hue;
use superzej_core::{repo, util, worktree};

// ---------------------------------------------------------------------------
// Wizard state machine
// ---------------------------------------------------------------------------

/// Which field of the single-plane form has focus, top-to-bottom. All fields
/// render at once; focus moves with Up/Down. The `Sandbox` field is skipped
/// (see [`NewWorktreeWizard::host_is_local`]) when the selected host is a
/// non-local placement — there the placement *is* the isolation boundary, so
/// the sandbox backend is managed by the host, not chosen here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Host,
    Sandbox,
    Program,
}

/// What a key delivered to the wizard meant. `PrepChosen` fires whenever the
/// user changes the host or sandbox selection, so the worker can start the
/// placement bring-up / container ensure while they pick a program; `Submit`
/// carries the full form and fires only from Enter on the Program list (Enter on
/// any other field advances focus rather than creating).
#[derive(Debug, Clone, PartialEq)]
pub enum WizardOutcome {
    /// The "+ add host…" row was chosen: the loop closes the wizard, opens the
    /// add-host input, and re-opens the wizard once the host exists.
    AddHost,
    Pending,
    Cancel,
    PrepChosen {
        env: String,
        sandbox: String,
    },
    Submit(WizardChoices),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WizardChoices {
    pub name: NameChoice,
    /// Selected execution environment / host (bare env name, passed to
    /// [`superzej_core::config::Config::resolve_env`]). `"default"` = implicit.
    pub env: String,
    pub sandbox: String,
    pub agent: String,
}

/// `Generated` = the prefill was accepted verbatim (the speculative worktree
/// already has that name); `Human` carries the typed tail (without the
/// configured branch prefix).
#[derive(Debug, Clone, PartialEq)]
pub enum NameChoice {
    Generated,
    Human(String),
}

/// The default execution-environment (host) name, matching the head of
/// [`Config::resolve_env`]'s precedence (repo `.superzej.*` `env =` →
/// `[sandbox] default_env` → `"default"`), so the wizard's Host row opens on
/// the same env a pane would resolve to.
pub(crate) fn default_env_name(cfg: &Config, repo_root: &Path) -> String {
    let pick = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    pick(&cfg.repo_env_name(repo_root))
        .or_else(|| pick(&cfg.repo_sandbox(repo_root).default_env))
        .unwrap_or_else(|| "default".to_string())
}

/// The Alt+w modal, a single-plane form: branch name (the configured prefix is
/// fixed chrome, the tail is editable), host (execution env), sandbox backend,
/// and program. All render at once; focus starts on the program list. Pure over
/// keys; the loop dispatches on [`WizardOutcome`].
/// Sentinel env key for the wizard's "+ add host…" row.
pub const ADD_HOST_KEY: &str = "__add_host__";

#[derive(Debug)]
pub struct NewWorktreeWizard {
    pub repo_slug: String,
    /// Repo root, kept so the add-host flow can re-open the wizard after.
    root: PathBuf,
    prefix: String,
    focus: Field,
    tail: String,
    name_edited: bool,
    name_checked: bool,
    /// (env key, label, is_local) — `is_local` gates whether the sandbox row is
    /// an editable choice or a host-managed static line.
    host_rows: Vec<(String, String, bool)>,
    host_sel: usize,
    /// Env key → host-readiness badge ("✓ ready" / "◐ …" / "✗ failed" /
    /// "○ new") for envs bound to a `[host.*]` entry; rendered dim after the
    /// host label. Empty unless the launcher provided one (hosts-as-resources).
    host_badges: std::collections::HashMap<String, String>,
    sandbox_rows: Vec<(String, String)>,
    sandbox_sel: usize,
    agent_rows: Vec<(String, String)>,
    agent_sel: usize,
}

impl NewWorktreeWizard {
    /// Opens instantly: the prefill is the pure (zero-git) candidate name;
    /// the worker's preflight refines it with a collision-free suggestion.
    pub fn new(repo_root: PathBuf, cfg: &Config) -> Self {
        let prefix = cfg.branch_prefix.clone();
        let candidate = worktree::candidate_name(cfg);
        let tail = candidate
            .strip_prefix(&prefix)
            .unwrap_or(&candidate)
            .to_string();
        // Host rows carry a local-ness flag: local means "pick a sandbox
        // backend below"; a non-local placement is its own boundary.
        let host_rows: Vec<(String, String, bool)> = crate::palette::build_env_palette(cfg)
            .into_iter()
            .map(|i| {
                let local = i.key == "default"
                    || cfg
                        .env
                        .get(&i.key)
                        .map(|e| matches!(e.placement, superzej_core::config::PlacementMode::Local))
                        .unwrap_or(true);
                (i.key, i.label, local)
            })
            .collect();
        let mut host_rows = host_rows;
        // Trailing "+ add host…" row: Enter on it opens the add-host input
        // (cycling onto it is inert — no PrepChosen fires for the sentinel).
        host_rows.push((ADD_HOST_KEY.to_string(), "+ add host…".to_string(), true));
        let default_env = default_env_name(cfg, &repo_root);
        let host_sel = host_rows
            .iter()
            .position(|(k, _, _)| *k == default_env)
            .unwrap_or(0);
        let sandbox_rows = crate::palette::build_sandbox_palette(cfg)
            .into_iter()
            .map(|i| {
                let key = i.key.strip_prefix("sandbox:").unwrap_or(&i.key).to_string();
                (key, i.label)
            })
            .collect();
        let agent_rows = crate::palette::build_agent_palette(cfg)
            .into_iter()
            .map(|i| (i.key, i.label))
            .collect();
        NewWorktreeWizard {
            repo_slug: repo::repo_slug(&repo_root),
            root: repo_root.clone(),
            prefix,
            focus: Field::Program,
            tail,
            name_edited: false,
            name_checked: false,
            host_rows,
            host_sel,
            host_badges: std::collections::HashMap::new(),
            sandbox_rows,
            sandbox_sel: 0,
            agent_rows,
            agent_sel: 0,
        }
    }

    /// The repo root this wizard creates worktrees under.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Attach per-env host-readiness badges (see
    /// [`crate::host_ui::wizard_host_badges`]); rendered after the host label.
    pub fn set_host_badges(&mut self, badges: std::collections::HashMap<String, String>) {
        self.host_badges = badges;
    }

    /// True when the selected host runs on the local machine (so a sandbox
    /// backend is a real choice). A non-local placement (ssh/k8s/provider) is
    /// its own isolation boundary — the sandbox field is host-managed.
    fn host_is_local(&self) -> bool {
        self.host_rows
            .get(self.host_sel)
            .map(|(_, _, local)| *local)
            .unwrap_or(true)
    }

    /// Current host env key (bare env name for `resolve_env`).
    fn host_key(&self) -> String {
        self.host_rows
            .get(self.host_sel)
            .map(|(k, _, _)| k.clone())
            .unwrap_or_else(|| "default".into())
    }

    /// Current sandbox backend key. A non-local host manages its own isolation,
    /// so we submit `"auto"` there and let the env's overlay govern.
    fn sandbox_key(&self) -> String {
        if !self.host_is_local() {
            return "auto".into();
        }
        self.sandbox_rows
            .get(self.sandbox_sel)
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| "auto".into())
    }

    /// Focus one field up (Name is the top). Skips Sandbox for a non-local host.
    fn focus_up(&mut self) {
        self.focus = match self.focus {
            Field::Name => Field::Name,
            Field::Host => Field::Name,
            Field::Sandbox => Field::Host,
            Field::Program if self.host_is_local() => Field::Sandbox,
            Field::Program => Field::Host,
        };
    }

    /// Focus one field down (Program is the bottom). Skips Sandbox for a
    /// non-local host.
    fn focus_down(&mut self) {
        self.focus = match self.focus {
            Field::Name => Field::Host,
            Field::Host if self.host_is_local() => Field::Sandbox,
            Field::Host => Field::Program,
            Field::Sandbox => Field::Program,
            Field::Program => Field::Program,
        };
    }

    /// The prep command for the current (host, sandbox) selection — the loop
    /// forwards it to the worker so bring-up overlaps the user's remaining time.
    fn prep_outcome(&self) -> WizardOutcome {
        if self.host_key() == ADD_HOST_KEY {
            return WizardOutcome::Pending; // sentinel row: nothing to bring up
        }
        WizardOutcome::PrepChosen {
            env: self.host_key(),
            sandbox: self.sandbox_key(),
        }
    }

    /// Build a `Submit` from the current selections, or `Pending` if the branch
    /// name is empty (the name field must not be blank).
    fn submit(&self) -> WizardOutcome {
        if self.host_key() == ADD_HOST_KEY {
            return WizardOutcome::AddHost;
        }
        if self.tail.trim().is_empty() {
            return WizardOutcome::Pending;
        }
        let name = if self.name_edited {
            NameChoice::Human(self.tail.clone())
        } else {
            NameChoice::Generated
        };
        WizardOutcome::Submit(WizardChoices {
            name,
            env: self.host_key(),
            sandbox: self.sandbox_key(),
            agent: self
                .agent_rows
                .get(self.agent_sel)
                .map(|(k, _)| k.clone())
                .unwrap_or_else(|| "shell".into()),
        })
    }

    /// The full branch-name candidate (prefix + tail) the worker speculates on.
    pub fn candidate(&self) -> String {
        format!("{}{}", self.prefix, self.tail)
    }

    /// Seed the wizard from a worktree template (item 54): override the branch
    /// prefix and pre-select the template's sandbox backend + agent so the
    /// common path is "accept and go". Unknown sandbox/agent values leave the
    /// default selection.
    pub fn apply_template(&mut self, tmpl: &superzej_core::config::WorktreeTemplate) {
        if let Some(prefix) = tmpl.branch_prefix.as_ref().filter(|p| !p.is_empty()) {
            self.prefix = prefix.clone();
        }
        if let Some(sb) = tmpl.sandbox.as_deref()
            && let Some(i) = self.sandbox_rows.iter().position(|(k, _)| k == sb)
        {
            self.sandbox_sel = i;
        }
        if let Some(agent) = tmpl.agent.as_deref()
            && let Some(i) = self.agent_rows.iter().position(|(k, _)| k == agent)
        {
            self.agent_sel = i;
        }
    }

    /// Adopt the worker's collision-free suggestion — only while the field is
    /// pristine (a typed name is never clobbered).
    pub fn apply_name_suggestion(&mut self, suggested: &str) {
        self.name_checked = true;
        if !self.name_edited {
            self.tail = suggested
                .strip_prefix(&self.prefix)
                .unwrap_or(suggested)
                .to_string();
        }
    }

    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> WizardOutcome {
        if mods.contains(Modifiers::CTRL) {
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => WizardOutcome::Cancel,
                _ => WizardOutcome::Pending,
            };
        }
        if mods.contains(Modifiers::ALT) || mods.contains(Modifiers::SUPER) {
            return WizardOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return WizardOutcome::Cancel;
        }
        // Enter is field-specific: it *creates* only on the Program list (the
        // terminal field the wizard opens on); on every other field it means
        // "confirm this, move on" and advances focus toward Program — so an
        // Enter while editing Name/Host/Sandbox never silently creates.
        match self.focus {
            // Text field: characters (including j/k/h/l) type literally; only
            // the arrows (and Enter, as "next") move focus.
            Field::Name => {
                match key {
                    KeyCode::Enter | KeyCode::DownArrow => self.focus_down(),
                    KeyCode::UpArrow => self.focus_up(),
                    KeyCode::Backspace => {
                        // Popping marks the field edited; `|=` keeps an earlier
                        // edit flag set and avoids a nested `if`.
                        self.name_edited |= self.tail.pop().is_some();
                    }
                    KeyCode::Char(c) => {
                        self.tail.push(*c);
                        self.name_edited = true;
                    }
                    _ => {}
                }
                WizardOutcome::Pending
            }
            // Inline cycle rows: ←/→ (or h/l) cycle the value; ↑/↓ (or k/j)
            // move focus. A changed value fires PrepChosen so the worker can
            // begin bring-up.
            Field::Host => match key {
                KeyCode::LeftArrow | KeyCode::Char('h') => {
                    if self.host_sel > 0 {
                        self.host_sel -= 1;
                        return self.prep_outcome();
                    }
                    WizardOutcome::Pending
                }
                KeyCode::RightArrow | KeyCode::Char('l') => {
                    if self.host_sel + 1 < self.host_rows.len() {
                        self.host_sel += 1;
                        return self.prep_outcome();
                    }
                    WizardOutcome::Pending
                }
                KeyCode::UpArrow | KeyCode::Char('k') => {
                    self.focus_up();
                    WizardOutcome::Pending
                }
                KeyCode::Enter if self.host_key() == ADD_HOST_KEY => WizardOutcome::AddHost,
                KeyCode::Enter | KeyCode::DownArrow | KeyCode::Char('j') => {
                    self.focus_down();
                    WizardOutcome::Pending
                }
                _ => WizardOutcome::Pending,
            },
            Field::Sandbox => match key {
                KeyCode::LeftArrow | KeyCode::Char('h') => {
                    if self.sandbox_sel > 0 {
                        self.sandbox_sel -= 1;
                        return self.prep_outcome();
                    }
                    WizardOutcome::Pending
                }
                KeyCode::RightArrow | KeyCode::Char('l') => {
                    if self.sandbox_sel + 1 < self.sandbox_rows.len() {
                        self.sandbox_sel += 1;
                        return self.prep_outcome();
                    }
                    WizardOutcome::Pending
                }
                KeyCode::UpArrow | KeyCode::Char('k') => {
                    self.focus_up();
                    WizardOutcome::Pending
                }
                KeyCode::Enter | KeyCode::DownArrow | KeyCode::Char('j') => {
                    self.focus_down();
                    WizardOutcome::Pending
                }
                _ => WizardOutcome::Pending,
            },
            // Full list: ↑/↓ (or k/j) move within it; ↑ at the top moves focus
            // up to the choice rows; Enter creates (this is the only field that
            // submits).
            Field::Program => match key {
                KeyCode::Enter => self.submit(),
                KeyCode::DownArrow | KeyCode::Char('j') => {
                    let max = self.agent_rows.len().saturating_sub(1);
                    self.agent_sel = self.agent_sel.saturating_add(1).min(max);
                    WizardOutcome::Pending
                }
                KeyCode::UpArrow | KeyCode::Char('k') => {
                    if self.agent_sel == 0 {
                        self.focus_up();
                    } else {
                        self.agent_sel -= 1;
                    }
                    WizardOutcome::Pending
                }
                _ => WizardOutcome::Pending,
            },
        }
    }

    /// Paint the single-plane form as a centered layer: name, host, sandbox,
    /// program list, and a footer hint.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let show_collision = self.focus == Field::Name && !self.name_checked && !self.name_edited;
        // name + host + sandbox + program header + the program list, plus the
        // transient collision line and a footer/gap.
        let body_rows = 4 + usize::from(show_collision) + self.agent_rows.len();
        let spec = LayerSpec {
            title: format!("new worktree — {}", self.repo_slug),
            badge: Some(" Alt+w ".into()),
            cols: 54,
            rows: body_rows + 2,
            anchor: Anchor::Center,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        // Label foreground: accent+bold for the focused field, faint otherwise.
        let label_fg = |focused: bool| {
            if focused {
                Tok::Slot(S::Accent)
            } else {
                Tok::Slot(S::Faint)
            }
        };
        let mut y = inner.y;

        // --- branch name (editable) ---------------------------------------
        let name_focused = self.focus == Field::Name;
        seg::draw_line(
            surface,
            inner.x,
            y,
            inner.cols,
            &Line::segs(vec![
                seg(label_fg(name_focused), "branch  ❯ ".to_string()).bold(),
                seg(Tok::Slot(S::Faint), self.prefix.clone()),
                seg(Tok::Slot(S::Text), self.tail.clone()),
                if name_focused {
                    seg(Tok::Slot(S::Accent), "▏")
                } else {
                    sp(0)
                },
            ]),
            panel,
        );
        y += 1;
        if show_collision {
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![
                    sp(10),
                    seg(Tok::Slot(S::Faint), "checking collisions…".to_string()),
                ]),
                panel,
            );
            y += 1;
        }

        // --- host (inline cycle) ------------------------------------------
        let host_focused = self.focus == Field::Host;
        let host_label = self
            .host_rows
            .get(self.host_sel)
            .map(|(_, l, _)| l.as_str())
            .unwrap_or("default");
        let mut host_segs = self.cycle_row("host   ", host_focused, host_label);
        // Host-readiness badge (hosts-as-resources): dim, after the label, for
        // envs bound to a `[host.*]` entry.
        if let Some(badge) = self.host_badges.get(&self.host_key()) {
            host_segs.push(seg(Tok::Slot(S::Dim), format!("  {badge}")));
        }
        seg::draw_line(
            surface,
            inner.x,
            y,
            inner.cols,
            &Line::segs(host_segs),
            panel,
        );
        y += 1;

        // --- sandbox (inline cycle when local; host-managed otherwise) -----
        let sb_focused = self.focus == Field::Sandbox;
        if self.host_is_local() {
            let sb_label = self
                .sandbox_rows
                .get(self.sandbox_sel)
                .map(|(_, l)| l.as_str())
                .unwrap_or("auto");
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(self.cycle_row("sandbox", sb_focused, sb_label)),
                panel,
            );
        } else {
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![
                    seg(Tok::Slot(S::Faint), "sandbox   ".to_string()),
                    seg(Tok::Slot(S::Dim), "· managed by host".to_string()),
                ]),
                panel,
            );
        }
        y += 1;

        // --- program (full list, the primary selection) -------------------
        let prog_focused = self.focus == Field::Program;
        seg::draw_line(
            surface,
            inner.x,
            y,
            inner.cols,
            &Line::segs(vec![
                seg(label_fg(prog_focused), "program".to_string()).bold(),
            ]),
            panel,
        );
        y += 1;
        for (row, (_, label)) in self.agent_rows.iter().enumerate() {
            let selected = row == self.agent_sel;
            let pad = if selected && prog_focused {
                Tok::SelAccent
            } else {
                panel
            };
            let marker = if selected {
                seg(Tok::Slot(S::Accent), "❯ ").bold()
            } else {
                sp(2)
            };
            let mut l = seg(
                if selected {
                    Tok::Slot(S::Text)
                } else {
                    Tok::Slot(S::Dim)
                },
                label.clone(),
            );
            if selected {
                l = l.bold();
            }
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![marker, l]),
                pad,
            );
            y += 1;
        }

        // Enter creates only on the Program list; elsewhere it advances focus.
        let enter_verb = if self.focus == Field::Program {
            "enter create"
        } else {
            "enter next"
        };
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows - 1,
            inner.cols,
            &Line::segs(vec![seg(
                Tok::Slot(S::Faint),
                format!("↑↓ move · ←→ change · {enter_verb} · esc cancel"),
            )]),
            panel,
        );
    }

    /// A `label  ‹ value ›` inline choice row; the chevrons appear only when the
    /// field is focused (signalling it is cyclable with ←/→).
    fn cycle_row(&self, label: &str, focused: bool, value: &str) -> Vec<Seg> {
        let fg = if focused {
            Tok::Slot(S::Accent)
        } else {
            Tok::Slot(S::Faint)
        };
        let mut segs = vec![seg(fg, format!("{label} ")).bold()];
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

// ---------------------------------------------------------------------------
// Creation progress
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateStep {
    ResolveBase,
    CreateWorktree,
    FinalizeName,
    SandboxPrep,
    Register,
    LaunchAgent,
}

impl CreateStep {
    const ALL: [CreateStep; 6] = [
        CreateStep::ResolveBase,
        CreateStep::CreateWorktree,
        CreateStep::FinalizeName,
        CreateStep::SandboxPrep,
        CreateStep::Register,
        CreateStep::LaunchAgent,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            CreateStep::ResolveBase => "resolve base",
            CreateStep::CreateWorktree => "create worktree",
            CreateStep::FinalizeName => "finalize name",
            CreateStep::SandboxPrep => "sandbox",
            CreateStep::Register => "register",
            CreateStep::LaunchAgent => "launch agent",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Skipped,
    Failed(String),
}

/// Events from the worker (and the spinner ticker) to the loop. Tagged with
/// the creation generation so a cancelled run's stragglers die on arrival.
#[derive(Debug)]
pub enum CreateEvent {
    /// Preflight result: resolved base + the collision-free name suggestion.
    Preflight {
        generation: u64,
        suggested: String,
    },
    Step {
        generation: u64,
        step: CreateStep,
        state: StepState,
        detail: Option<String>,
    },
    Tick {
        generation: u64,
    },
    Done {
        generation: u64,
        payload: Box<CreatedWorktree>,
    },
    /// Terminal failure: the worker cleaned up (best effort) and exited.
    Failed {
        generation: u64,
        step: CreateStep,
        error: String,
    },
}

/// Everything the loop needs to adopt the finished worktree: the group/tab
/// names, and a fully-resolved launch spec so the only loop-side work is the
/// (fast) openpty+exec.
#[derive(Debug)]
pub struct CreatedWorktree {
    pub tab: String,
    pub branch: String,
    pub path: String,
    pub agent: String,
    pub spec: crate::agent::LaunchSpec,
}

/// Commands from the loop (wizard decisions) to the worker.
#[derive(Debug)]
pub enum WizardCmd {
    /// The user settled on a (host env, sandbox) pair — start bring-up/ensure
    /// early so it overlaps the rest of the wizard.
    PrepChosen {
        env: String,
        sandbox: String,
    },
    Submit(WizardChoices),
    Cancel,
}

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// The progress overlay model. Created (hidden) at Alt+w so pre-submit events
/// (the speculative create) accumulate; revealed when the wizard submits.
#[derive(Debug)]
pub struct CreationProgress {
    pub generation: u64,
    pub branch: String,
    rows: Vec<(CreateStep, StepState, Option<String>)>,
    tick: u64,
    pub revealed: bool,
    pub failed: bool,
    pub ticker_alive: Arc<AtomicBool>,
}

impl CreationProgress {
    pub fn new(generation: u64, branch: String) -> Self {
        CreationProgress {
            generation,
            branch,
            rows: CreateStep::ALL
                .iter()
                .map(|s| (*s, StepState::Pending, None))
                .collect(),
            tick: 0,
            revealed: false,
            failed: false,
            ticker_alive: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn apply(&mut self, step: CreateStep, state: StepState, detail: Option<String>) {
        if matches!(state, StepState::Failed(_)) {
            self.failed = true;
        }
        if let Some(row) = self.rows.iter_mut().find(|(s, _, _)| *s == step) {
            row.1 = state;
            if detail.is_some() {
                row.2 = detail;
            }
        }
    }

    #[cfg(test)]
    pub fn state(&self, step: CreateStep) -> &StepState {
        &self
            .rows
            .iter()
            .find(|(s, _, _)| *s == step)
            .expect("all steps present")
            .1
    }

    pub fn bump_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    pub fn stop_ticker(&self) {
        self.ticker_alive.store(false, Ordering::Relaxed);
    }

    /// Paint as a centered layer (topmost): one row per step, then a footer.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        // The error row (when failed) renders under its step, so size for it.
        let err_rows = usize::from(self.failed);
        let spec = LayerSpec {
            title: format!("creating {}", self.branch),
            badge: Some(" Alt+w ".into()),
            cols: 54,
            rows: self.rows.len() + 1 + err_rows,
            anchor: Anchor::Center,
            border: if self.failed {
                Tok::Hue(Hue::Red)
            } else {
                Tok::Slot(S::Accent)
            },
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        let mut y = inner.y;
        let mut error: Option<String> = None;
        for (step, state, detail) in &self.rows {
            let (glyph, glyph_tok, text_tok) = match state {
                StepState::Pending => ("○", Tok::Slot(S::Faint), Tok::Slot(S::Faint)),
                StepState::Running => (
                    SPINNER[(self.tick % SPINNER.len() as u64) as usize],
                    Tok::Slot(S::Accent),
                    Tok::Slot(S::Text),
                ),
                StepState::Done => ("✓", Tok::Hue(Hue::Green), Tok::Slot(S::Dim)),
                StepState::Skipped => ("–", Tok::Slot(S::Faint), Tok::Slot(S::Faint)),
                StepState::Failed(e) => {
                    error = Some(e.clone());
                    ("✗", Tok::Hue(Hue::Red), Tok::Slot(S::Text))
                }
            };
            let mut segs = vec![
                sp(1),
                seg(glyph_tok, glyph.to_string()).bold(),
                sp(1),
                seg(text_tok, step.label().to_string()),
            ];
            if let Some(d) = detail {
                segs.push(sp(2));
                segs.push(seg(Tok::Slot(S::Ghost), format!("({d})")));
            }
            seg::draw_line(surface, inner.x, y, inner.cols, &Line::segs(segs), panel);
            y += 1;
        }
        if let Some(e) = error {
            let mut e = e.replace('\n', " ");
            if e.chars().count() > inner.cols.saturating_sub(4) {
                e = e
                    .chars()
                    .take(inner.cols.saturating_sub(5))
                    .collect::<String>()
                    + "…";
            }
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![sp(3), seg(Tok::Hue(Hue::Red), e)]),
                panel,
            );
            y += 1;
        }
        let footer = if self.failed {
            "esc dismiss"
        } else {
            "esc hide · creation continues"
        };
        seg::draw_line(
            surface,
            inner.x,
            y,
            inner.cols,
            &Line::segs(vec![sp(1), seg(Tok::Slot(S::Faint), footer.to_string())]),
            panel,
        );
    }
}

/// Spawn the spinner ticker: ~8 fps while `alive`, each tick an event + wake.
/// The thread exits within one period of the loop clearing the flag (done,
/// failed, or overlay hidden) — no free-running timer survives the overlay.
pub fn spawn_ticker(
    generation: u64,
    alive: Arc<AtomicBool>,
    events: tokio::sync::mpsc::UnboundedSender<CreateEvent>,
    notify: impl Fn() + Send + 'static,
) {
    alive.store(true, Ordering::Relaxed);
    std::thread::spawn(move || {
        while alive.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(120));
            if events.send(CreateEvent::Tick { generation }).is_err() {
                break;
            }
            notify();
        }
    });
}

// ---------------------------------------------------------------------------
// Worker pipeline
// ---------------------------------------------------------------------------

/// Per-invocation inputs for [`run_worker`].
pub struct WorkerCtx {
    pub cfg: Config,
    pub repo_root: PathBuf,
    /// Full branch-name candidate (prefix included) from the wizard prefill.
    pub candidate: String,
    pub generation: u64,
    /// Test seam: `Some(path)` opens the DB there instead of the default
    /// state dir (this shell often runs inside a live superzej).
    pub db_path: Option<PathBuf>,
    /// Fork source: when `Some(branch)`, the new worktree branches from this
    /// ref instead of the configured/auto-resolved base (sidebar "fork
    /// worktree", item 52). Empty/None falls back to `resolve_base`.
    pub base_override: Option<String>,
}

/// The blocking half of worktree creation, run on a `spawn_blocking` thread:
/// preflight (collision-free name + base), speculative `git worktree add`
/// under the suggested name, then a command loop driven by the wizard —
/// sandbox prep on `PrepChosen`, rename/register/compose on `Submit`,
/// cleanup on `Cancel`. Every transition is an event + `notify()`.
pub fn run_worker(
    ctx: WorkerCtx,
    cmds: std::sync::mpsc::Receiver<WizardCmd>,
    events: tokio::sync::mpsc::UnboundedSender<CreateEvent>,
    notify: impl Fn(),
) {
    let started = std::time::Instant::now();
    let generation = ctx.generation;
    let root = ctx.repo_root.as_path();
    let cfg = &ctx.cfg;
    let send = |ev: CreateEvent| {
        let _ = events.send(ev);
        notify();
    };
    let step = |s: CreateStep, state: StepState, detail: Option<String>| {
        tracing::info!(
            target: "szhost::worktree_create",
            since_ms = started.elapsed().as_millis() as u64,
            step = s.label(),
            state = ?state,
            "step"
        );
        send(CreateEvent::Step {
            generation,
            step: s,
            state,
            detail,
        });
    };
    let fail = |s: CreateStep, error: String| {
        step(s, StepState::Failed(error.clone()), None);
        send(CreateEvent::Failed {
            generation,
            step: s,
            error,
        });
    };

    // --- preflight: taken names + base, then the collision-free suggestion.
    step(CreateStep::ResolveBase, StepState::Running, None);
    let taken = worktree::BranchSet::load(root);
    // Fork (item 52) branches from the chosen source ref; otherwise the
    // configured/auto-resolved base.
    let base = match ctx.base_override.as_deref() {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => worktree::resolve_base(root, cfg),
    };
    if util::git_out(root, &["rev-parse", "--verify", "--quiet", &base]).is_none() {
        fail(
            CreateStep::ResolveBase,
            format!("'{base}' has no commits yet — make an initial commit first"),
        );
        return;
    }
    let mut branch = worktree::dedupe(&ctx.candidate, &taken);
    send(CreateEvent::Preflight {
        generation,
        suggested: branch.clone(),
    });
    step(CreateStep::ResolveBase, StepState::Done, Some(base.clone()));

    // --- speculative create under the suggested name (the dominant cost —
    // overlaps the checkout with the user's wizard time).
    step(CreateStep::CreateWorktree, StepState::Running, None);
    let mut path = worktree::worktree_path(root, &branch, cfg);
    if let Err(e) = worktree::add_checked(root, &branch, &base, &path, cfg) {
        worktree::remove(root, &path, &branch, true);
        fail(CreateStep::CreateWorktree, e);
        return;
    }
    step(CreateStep::CreateWorktree, StepState::Done, None);

    // --- command loop: the wizard drives the rest.
    let slug = repo::repo_slug(root);
    // Keyed on (env, backend): both the host env and the sandbox backend feed
    // the placement/isolation bring-up, so a change to either invalidates a
    // prior prep.
    let mut prepped: Option<(String, String, crate::agent::SandboxOutcome)> = None;
    let prep = |env: &str,
                backend: &str,
                wt: &Path,
                scope: crate::agent::SandboxScope|
     -> anyhow::Result<crate::agent::SandboxOutcome> {
        let wt_s = wt.to_string_lossy();
        let loc = GitLoc::from_db(&wt_s, None);
        crate::agent::prepare_sandbox_env(cfg, root, &wt_s, &loc, Some(backend), scope, Some(env))
    };
    // The auto chain's host fallback is visible in the step detail; an
    // explicit choice that can't be honored errors instead (no silent host
    // fallback) and fails the creation below.
    let sandbox_detail = |o: &crate::agent::SandboxOutcome| -> String {
        if o.spec.is_none() && !o.warnings.is_empty() {
            format!("{} — fallback", o.backend_label)
        } else {
            o.backend_label.clone()
        }
    };
    let choices = loop {
        match cmds.recv() {
            // The loop dropped the sender without a verdict (shutdown): leave
            // the worktree in place — resurrect picks it up next session.
            Err(_) => return,
            Ok(WizardCmd::Cancel) => {
                worktree::remove(root, &path, &branch, true);
                tracing::info!(
                    target: "szhost::worktree_create",
                    since_ms = started.elapsed().as_millis() as u64,
                    "cancelled — speculative worktree removed"
                );
                return;
            }
            Ok(WizardCmd::PrepChosen { env, sandbox }) => {
                if prepped.as_ref().map(|(e, b, _)| (e.as_str(), b.as_str()))
                    == Some((env.as_str(), sandbox.as_str()))
                {
                    continue; // re-fired with the same (env, backend)
                }
                step(CreateStep::SandboxPrep, StepState::Running, None);
                match prep(&env, &sandbox, &path, crate::agent::SandboxScope::Shell) {
                    Ok(outcome) => {
                        step(
                            CreateStep::SandboxPrep,
                            StepState::Done,
                            Some(sandbox_detail(&outcome)),
                        );
                        prepped = Some((env, sandbox, outcome));
                    }
                    Err(e) => {
                        worktree::remove(root, &path, &branch, true);
                        fail(CreateStep::SandboxPrep, e.to_string());
                        return;
                    }
                }
            }
            Ok(WizardCmd::Submit(choices)) => break choices,
        }
    };

    // --- finalize name: accepted prefill is free; a typed name is a
    // metadata-only branch rename + worktree move.
    match &choices.name {
        NameChoice::Generated => {
            step(CreateStep::FinalizeName, StepState::Skipped, None);
        }
        NameChoice::Human(tail) => {
            let want = worktree::dedupe(
                &worktree::human_base(tail, cfg),
                &worktree::BranchSet::load(root),
            );
            if want == branch {
                step(CreateStep::FinalizeName, StepState::Skipped, None);
            } else {
                step(CreateStep::FinalizeName, StepState::Running, None);
                match worktree::rename(root, &path, &branch, &want, cfg) {
                    Ok(new_path) => {
                        branch = want;
                        path = new_path;
                        // A sandbox container is keyed by the worktree path —
                        // re-ensure for the moved path (cheap next to a checkout).
                        if let Some((env, backend, outcome)) = prepped.as_mut()
                            && outcome.spec.is_some()
                        {
                            let env = env.clone();
                            let backend = backend.clone();
                            step(CreateStep::SandboxPrep, StepState::Running, None);
                            match prep(&env, &backend, &path, crate::agent::SandboxScope::Shell) {
                                Ok(redo) => {
                                    step(
                                        CreateStep::SandboxPrep,
                                        StepState::Done,
                                        Some(sandbox_detail(&redo)),
                                    );
                                    *outcome = redo;
                                }
                                Err(e) => {
                                    worktree::remove(root, &path, &branch, true);
                                    fail(CreateStep::SandboxPrep, e.to_string());
                                    return;
                                }
                            }
                        }
                        step(
                            CreateStep::FinalizeName,
                            StepState::Done,
                            Some(branch.clone()),
                        );
                    }
                    Err(why) => {
                        // Keep the generated name rather than failing a finished
                        // checkout; the overlay shows what happened.
                        step(
                            CreateStep::FinalizeName,
                            StepState::Done,
                            Some(format!("rename failed — kept {branch} ({why})")),
                        );
                    }
                }
            }
        }
    }

    // Reuse the speculative prep only when it matches the submitted (env,
    // backend); otherwise (no prep, or the user changed host/sandbox after the
    // last PrepChosen) prepare now with the submitted choice.
    let reuse = prepped
        .as_ref()
        .map(|(e, b, _)| *e == choices.env && *b == choices.sandbox)
        .unwrap_or(false);
    let (env_used, backend_label, mut sandbox) = if reuse {
        prepped.expect("reuse implies prepped is Some")
    } else {
        step(CreateStep::SandboxPrep, StepState::Running, None);
        match prep(
            &choices.env,
            &choices.sandbox,
            &path,
            crate::agent::SandboxScope::Shell,
        ) {
            Ok(outcome) => {
                step(
                    CreateStep::SandboxPrep,
                    StepState::Done,
                    Some(sandbox_detail(&outcome)),
                );
                (choices.env.clone(), choices.sandbox.clone(), outcome)
            }
            Err(e) => {
                worktree::remove(root, &path, &branch, true);
                fail(CreateStep::SandboxPrep, e.to_string());
                return;
            }
        }
    };

    // Bouncer (opt-in): the speculative prep above used the worktree shell scope.
    // Now that the agent is chosen, re-resolve under the sealed `agent_profile`
    // scope so the agent gets (and ensures) its own hardened container.
    if crate::agent::launch_scope(cfg, &choices.agent) == crate::agent::SandboxScope::Agent {
        step(CreateStep::SandboxPrep, StepState::Running, None);
        match prep(
            &env_used,
            &backend_label,
            &path,
            crate::agent::SandboxScope::Agent,
        ) {
            Ok(redo) => {
                step(
                    CreateStep::SandboxPrep,
                    StepState::Done,
                    Some(format!("{} (sealed agent)", sandbox_detail(&redo))),
                );
                sandbox = redo;
            }
            Err(e) => {
                worktree::remove(root, &path, &branch, true);
                fail(CreateStep::SandboxPrep, e.to_string());
                return;
            }
        }
    }

    // --- register: one DB open for the whole pipeline. put_worktree must
    // precede the sandbox/agent updates (they are bare UPDATEs).
    step(CreateStep::Register, StepState::Running, None);
    let tab = repo::branch_tab(&slug, &branch);
    let path_s = path.to_string_lossy().into_owned();
    let db = match &ctx.db_path {
        Some(p) => Db::open_at(p),
        None => Db::open(),
    };
    match db {
        Ok(db) => {
            let root_s = root.to_string_lossy();
            // For a managed-provider env, persist the `GitLoc::Provider` location
            // so the chrome's git/fs reads route into the sandbox.
            let location = sandbox.location.as_deref();
            if let Err(e) = db.put_worktree(&tab, &root_s, &path_s, &branch, location, None) {
                fail(CreateStep::Register, format!("db: {e}"));
                return;
            }
            let _ = db.set_worktree_sandbox(&path_s, &sandbox.backend_label);
            let _ = db.set_worktree_agent(&path_s, &choices.agent);
            // Persist the chosen host only when it DIFFERS from the ambient
            // default this worktree would otherwise inherit (repo `.superzej.*`
            // env → global `[sandbox] default_env` → "default"). A divergent
            // choice — including an explicit "default" against a provider
            // ambient default — is pinned so every later re-resolution
            // (`effective_env` → `resolve_env(Some(..))`) reproduces the
            // wizard's placement instead of falling through to the ambient
            // sprite. A choice equal to the ambient default stays NULL (clean
            // inherit).
            let ambient = default_env_name(cfg, root);
            if choices.env != ambient {
                let _ = db.set_worktree_env(&path_s, &choices.env);
            }
            step(CreateStep::Register, StepState::Done, None);
        }
        Err(e) => {
            fail(CreateStep::Register, format!("db: {e}"));
            return;
        }
    }

    // Host-side setup for the finalized worktree (`path` is now settled past any
    // rename/move): run the one-time `[sandbox] prepare` hooks and warm the
    // `direnv` cache so the first pane's in-sandbox direnv replays it read-only
    // instead of failing on the read-only store. Both off-loop and self-gating.
    superzej_core::sandbox::run_prepare(&path, &cfg.sandbox.prepare);
    crate::agent::warm_direnv(cfg, &path);

    // --- compose the launch spec (pure); the loop does the openpty+exec.
    // Bouncer env (proxy + tool override) rides the sandbox's env_overrides; a
    // host fallback gets the proxy vars on the pane env instead.
    let bouncer = crate::agent::apply_bouncer_launch(cfg, &path_s, &choices.agent, &mut sandbox);
    let loc = GitLoc::from_db(&path_s, None);
    let mut spec =
        crate::agent::compose_spec(cfg, &path_s, Some(&branch), &choices.agent, &loc, &sandbox);
    if sandbox.spec.is_none() {
        spec.env.extend(bouncer.host_env);
    }
    tracing::info!(
        target: "szhost::worktree_create",
        since_ms = started.elapsed().as_millis() as u64,
        branch = %branch,
        "ready"
    );
    send(CreateEvent::Done {
        generation,
        payload: Box::new(CreatedWorktree {
            tab,
            branch,
            path: path_s,
            agent: choices.agent,
            spec,
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::config::WorktreeMode;

    fn key(w: &mut NewWorktreeWizard, k: KeyCode) -> WizardOutcome {
        w.handle_key(&k, Modifiers::NONE)
    }

    fn test_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.sandbox.backend = superzej_core::config::SandboxBackend::None;
        cfg.worktree_mode = WorktreeMode::InRepo;
        cfg
    }

    // test code: fixture setup, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sz-wiz-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t.t"],
            &["config", "user.name", "t"],
            &["commit", "--allow-empty", "-q", "-m", "init"],
        ] {
            assert!(util::git_cmd(&dir).args(args).status().unwrap().success());
        }
        dir
    }

    /// Run the worker synchronously with the wizard decisions pre-queued
    /// (the worker drains the command channel in order), collecting events.
    fn drive_worker(
        repo: &Path,
        candidate: &str,
        cmds: Vec<WizardCmd>,
        db_path: &Path,
    ) -> Vec<CreateEvent> {
        drive_worker_cfg(test_cfg(), repo, candidate, cmds, db_path)
    }

    /// Like [`drive_worker`] but with a caller-supplied config (e.g. to set a
    /// non-local `[sandbox] default_env` ambient).
    fn drive_worker_cfg(
        cfg: Config,
        repo: &Path,
        candidate: &str,
        cmds: Vec<WizardCmd>,
        db_path: &Path,
    ) -> Vec<CreateEvent> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        for c in cmds {
            cmd_tx.send(c).unwrap();
        }
        drop(cmd_tx);
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        run_worker(
            WorkerCtx {
                cfg,
                repo_root: repo.to_path_buf(),
                candidate: candidate.into(),
                generation: 7,
                db_path: Some(db_path.to_path_buf()),
                base_override: None,
            },
            cmd_rx,
            ev_tx,
            || {},
        );
        let mut out = Vec::new();
        while let Ok(ev) = ev_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn done_payload(events: &[CreateEvent]) -> Option<&CreatedWorktree> {
        events.iter().find_map(|e| match e {
            CreateEvent::Done { payload, .. } => Some(payload.as_ref()),
            _ => None,
        })
    }

    #[test]
    fn wizard_walkthrough_generated_name() {
        let cfg = test_cfg();
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        assert!(w.candidate().starts_with(&cfg.branch_prefix));
        assert!(w.candidate().len() > cfg.branch_prefix.len());

        // Opens focused on the program list, so Enter creates straight away
        // (the accept-defaults fast path).
        let submit = key(&mut w, KeyCode::Enter);
        let WizardOutcome::Submit(choices) = submit else {
            panic!("expected Submit, got {submit:?}");
        };
        assert_eq!(choices.name, NameChoice::Generated);
        assert!(!choices.env.is_empty());
        assert!(!choices.sandbox.is_empty());
        assert!(!choices.agent.is_empty());
    }

    #[test]
    fn enter_advances_on_edit_fields_and_creates_only_on_program() {
        let cfg = test_cfg();
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);

        // Walk up to the Name field (Program → Sandbox → Host → Name).
        key(&mut w, KeyCode::UpArrow);
        key(&mut w, KeyCode::UpArrow);
        key(&mut w, KeyCode::UpArrow);
        assert_eq!(w.focus, Field::Name);

        // Enter on Name advances to Host — it does not create.
        assert_eq!(key(&mut w, KeyCode::Enter), WizardOutcome::Pending);
        assert_eq!(w.focus, Field::Host);

        // Enter on Host advances to Sandbox (local default keeps it in the ring).
        assert_eq!(key(&mut w, KeyCode::Enter), WizardOutcome::Pending);
        assert_eq!(w.focus, Field::Sandbox);

        // Enter on Sandbox advances to Program.
        assert_eq!(key(&mut w, KeyCode::Enter), WizardOutcome::Pending);
        assert_eq!(w.focus, Field::Program);

        // Enter on the Program list is the only Enter that creates.
        assert!(matches!(
            key(&mut w, KeyCode::Enter),
            WizardOutcome::Submit(_)
        ));
    }

    #[test]
    fn typing_marks_edited_and_blocks_suggestion() {
        let cfg = test_cfg();
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        // Focus starts on Program; walk up to the Name field (Sandbox, Host,
        // Name — a local default host keeps the Sandbox row in the ring).
        key(&mut w, KeyCode::UpArrow);
        key(&mut w, KeyCode::UpArrow);
        key(&mut w, KeyCode::UpArrow);

        // j/k/h/l type literally into the branch name (no longer navigation).
        key(&mut w, KeyCode::Char('j'));
        key(&mut w, KeyCode::Char('x'));
        assert!(w.candidate().ends_with("jx"), "j typed into the name");
        let typed = w.candidate();
        w.apply_name_suggestion("sz/other-name");
        assert_eq!(w.candidate(), typed, "typed name never clobbered");

        // Enter on the Name field advances rather than creating; walk down to
        // the Program list, where Enter submits the typed (Human) name.
        key(&mut w, KeyCode::DownArrow);
        key(&mut w, KeyCode::DownArrow);
        key(&mut w, KeyCode::DownArrow);
        assert_eq!(w.focus, Field::Program);
        let WizardOutcome::Submit(choices) = key(&mut w, KeyCode::Enter) else {
            panic!("expected Submit");
        };
        assert!(matches!(choices.name, NameChoice::Human(_)));
    }

    #[test]
    fn pristine_field_adopts_suggestion() {
        let cfg = test_cfg();
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        w.apply_name_suggestion("sz/calm-otter-3");
        assert_eq!(w.candidate(), "sz/calm-otter-3");
    }

    #[test]
    fn empty_name_refuses_to_advance() {
        let cfg = test_cfg();
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        // Walk up to the Name field.
        key(&mut w, KeyCode::UpArrow);
        key(&mut w, KeyCode::UpArrow);
        key(&mut w, KeyCode::UpArrow);

        for _ in 0..64 {
            key(&mut w, KeyCode::Backspace);
        }
        // Walk down to the Program list; an empty branch name still refuses to
        // create there (submit() guards it). Esc still cancels.
        key(&mut w, KeyCode::DownArrow);
        key(&mut w, KeyCode::DownArrow);
        key(&mut w, KeyCode::DownArrow);
        assert_eq!(w.focus, Field::Program);
        assert_eq!(key(&mut w, KeyCode::Enter), WizardOutcome::Pending);
        assert_eq!(key(&mut w, KeyCode::Escape), WizardOutcome::Cancel);
    }

    #[test]
    fn esc_cancels_from_any_field_and_ctrl_c_cancels_anywhere() {
        let cfg = test_cfg();
        // Escape cancels from every field along the focus ring.
        for ups in 0..=3 {
            let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
            for _ in 0..ups {
                key(&mut w, KeyCode::UpArrow);
            }
            assert_eq!(key(&mut w, KeyCode::Escape), WizardOutcome::Cancel);
        }

        // Ctrl+c cancels regardless of focus.
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        assert_eq!(
            w.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            WizardOutcome::Cancel
        );
    }

    #[test]
    fn host_defaults_and_cycles() {
        let cfg = test_cfg();
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        // No named envs ⇒ the default host is the implicit "default".
        assert_eq!(w.host_key(), "default");
        assert!(w.host_is_local());
        // Focus the Host row and cycle: with only one host, → is a no-op; with
        // named envs it would return PrepChosen. Assert the local-only case.
        key(&mut w, KeyCode::UpArrow); // Program -> Sandbox
        key(&mut w, KeyCode::UpArrow); // Sandbox -> Host
        assert_eq!(key(&mut w, KeyCode::RightArrow), WizardOutcome::Pending);
    }

    #[test]
    fn non_local_host_skips_sandbox_in_focus_ring() {
        let mut cfg = test_cfg();
        cfg.env.insert(
            "remote".into(),
            superzej_core::config::EnvConfig {
                placement: superzej_core::config::PlacementMode::Ssh,
                ssh: superzej_core::config::EnvSshConfig {
                    host: "build-box".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        // Make the ssh env the default host so it opens selected.
        cfg.sandbox.default_env = "remote".into();

        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        assert_eq!(w.host_key(), "remote");
        assert!(!w.host_is_local(), "ssh host is non-local");
        // Program → up skips Sandbox and lands on Host directly.
        key(&mut w, KeyCode::UpArrow);
        assert_eq!(w.focus, Field::Host);
        // Enter on Host advances (skipping the host-managed Sandbox) to Program
        // rather than creating.
        assert_eq!(key(&mut w, KeyCode::Enter), WizardOutcome::Pending);
        assert_eq!(w.focus, Field::Program);
        // A second Enter on the Program list submits: the non-local env is
        // carried and sandbox is the host-managed sentinel.
        let WizardOutcome::Submit(choices) = key(&mut w, KeyCode::Enter) else {
            panic!("expected Submit");
        };
        assert_eq!(choices.env, "remote");
        assert_eq!(choices.sandbox, "auto");
    }

    #[test]
    fn progress_tracks_steps_and_failure() {
        let mut cp = CreationProgress::new(1, "sz/x".into());
        assert_eq!(*cp.state(CreateStep::CreateWorktree), StepState::Pending);
        cp.apply(CreateStep::CreateWorktree, StepState::Running, None);
        cp.apply(
            CreateStep::CreateWorktree,
            StepState::Done,
            Some("d".into()),
        );
        assert_eq!(*cp.state(CreateStep::CreateWorktree), StepState::Done);
        assert!(!cp.failed);
        cp.apply(CreateStep::Register, StepState::Failed("boom".into()), None);
        assert!(cp.failed);
        cp.ticker_alive.store(true, Ordering::Relaxed);
        cp.stop_ticker();
        assert!(!cp.ticker_alive.load(Ordering::Relaxed));
    }

    #[test]
    fn worker_creates_speculatively_and_finishes_on_submit() {
        let repo = temp_repo("happy");
        let db = repo.join("state/superzej.db");
        let events = drive_worker(
            &repo,
            "sz/test-one",
            vec![
                WizardCmd::PrepChosen {
                    env: "default".into(),
                    sandbox: "host".into(),
                },
                WizardCmd::Submit(WizardChoices {
                    name: NameChoice::Generated,
                    env: "default".into(),
                    sandbox: "host".into(),
                    agent: "shell".into(),
                }),
            ],
            &db,
        );
        let p = done_payload(&events).expect("Done event");
        assert_eq!(p.branch, "sz/test-one");
        assert!(Path::new(&p.path).is_dir(), "worktree on disk");
        assert!(!p.spec.argv.is_empty());
        // The speculative create finished before the submit was processed:
        // CreateWorktree Done precedes any SandboxPrep event.
        let order: Vec<&CreateStep> = events
            .iter()
            .filter_map(|e| match e {
                CreateEvent::Step { step, state, .. } if !matches!(state, StepState::Running) => {
                    Some(step)
                }
                _ => None,
            })
            .collect();
        let pos = |s: CreateStep| order.iter().position(|x| **x == s).unwrap();
        assert!(pos(CreateStep::CreateWorktree) < pos(CreateStep::SandboxPrep));
        assert!(pos(CreateStep::SandboxPrep) < pos(CreateStep::Register));
        // Registered in the isolated DB with the worktree's agent + backend.
        let db = Db::open_at(&db).unwrap();
        let rows = db.worktrees().unwrap();
        let row = rows.iter().find(|w| w.worktree == p.path).expect("db row");
        assert_eq!(row.branch, "sz/test-one");
        // The implicit "default" host is stored as NULL (not a literal).
        assert_eq!(row.env_name, None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn worker_persists_selected_host_env() {
        let repo = temp_repo("env");
        let db = repo.join("state/superzej.db");
        let events = drive_worker(
            &repo,
            "sz/env-one",
            vec![
                WizardCmd::PrepChosen {
                    env: "myenv".into(),
                    sandbox: "host".into(),
                },
                WizardCmd::Submit(WizardChoices {
                    name: NameChoice::Generated,
                    env: "myenv".into(),
                    sandbox: "host".into(),
                    agent: "shell".into(),
                }),
            ],
            &db,
        );
        let p = done_payload(&events).expect("Done event");
        // A non-default host is persisted verbatim on the worktree row.
        let db = Db::open_at(&db).unwrap();
        let rows = db.worktrees().unwrap();
        let row = rows.iter().find(|w| w.worktree == p.path).expect("db row");
        assert_eq!(row.env_name.as_deref(), Some("myenv"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn worker_persists_default_when_ambient_is_provider() {
        use superzej_core::config::{EnvConfig, PlacementMode};
        let repo = temp_repo("env-provider-default");
        let db = repo.join("state/superzej.db");
        // Ambient default is a provider (sprite) env; the user explicitly picks
        // the local "host default" row. That divergent "default" must be pinned
        // so later re-resolution stays local instead of falling through to the
        // sprite.
        let mut cfg = test_cfg();
        cfg.sandbox.default_env = "sprite".into();
        cfg.env.insert(
            "sprite".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                ..Default::default()
            },
        );
        let events = drive_worker_cfg(
            cfg,
            &repo,
            "sz/local-pick",
            vec![
                WizardCmd::PrepChosen {
                    env: "default".into(),
                    sandbox: "host".into(),
                },
                WizardCmd::Submit(WizardChoices {
                    name: NameChoice::Generated,
                    env: "default".into(),
                    sandbox: "host".into(),
                    agent: "shell".into(),
                }),
            ],
            &db,
        );
        let p = done_payload(&events).expect("Done event");
        let db = Db::open_at(&db).unwrap();
        let rows = db.worktrees().unwrap();
        let row = rows.iter().find(|w| w.worktree == p.path).expect("db row");
        // Diverges from the sprite ambient default ⇒ "default" is persisted, so
        // effective_env → resolve_env(Some("default")) resolves to Local.
        assert_eq!(row.env_name.as_deref(), Some("default"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn worker_renames_on_human_name() {
        let repo = temp_repo("rename");
        let db = repo.join("state/superzej.db");
        let events = drive_worker(
            &repo,
            "sz/generated-x",
            vec![
                WizardCmd::PrepChosen {
                    env: "default".into(),
                    sandbox: "host".into(),
                },
                WizardCmd::Submit(WizardChoices {
                    name: NameChoice::Human("My Fix".into()),
                    env: "default".into(),
                    sandbox: "host".into(),
                    agent: "shell".into(),
                }),
            ],
            &db,
        );
        let p = done_payload(&events).expect("Done event");
        assert_eq!(p.branch, "sz/my-fix");
        assert!(Path::new(&p.path).is_dir());
        assert!(p.path.contains("my-fix"));
        // The speculative branch was renamed, not duplicated.
        let set = worktree::BranchSet::load(&repo);
        assert!(set.taken("sz/my-fix"));
        assert!(!set.taken("sz/generated-x"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn worker_cancel_removes_speculative_worktree() {
        let repo = temp_repo("cancel");
        let db = repo.join("state/superzej.db");
        let events = drive_worker(&repo, "sz/doomed", vec![WizardCmd::Cancel], &db);
        assert!(done_payload(&events).is_none());
        let set = worktree::BranchSet::load(&repo);
        assert!(!set.taken("sz/doomed"), "branch cleaned up");
        assert!(
            !repo.join(".worktrees/sz-doomed").exists(),
            "worktree dir cleaned up"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    // test code: fixture setup, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn worker_fails_cleanly_without_commits() {
        let dir = std::env::temp_dir().join(format!("sz-wiz-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            util::git_cmd(&dir)
                .args(["init", "-q", "-b", "main"])
                .status()
                .unwrap()
                .success()
        );
        let db = dir.join("state/superzej.db");
        let events = drive_worker(&dir, "sz/x", vec![], &db);
        assert!(done_payload(&events).is_none());
        assert!(events.iter().any(|e| matches!(
            e,
            CreateEvent::Failed {
                step: CreateStep::ResolveBase,
                ..
            }
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_smoke_wizard_and_progress() {
        let cfg = test_cfg();
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        let text = |s: &mut Surface| -> String {
            s.screen_cells()
                .iter()
                .map(|row| row.iter().map(|c| c.str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        let mut s = Surface::new(80, 24);
        w.render(&mut s, screen);
        let frame = text(&mut s);
        assert!(frame.contains("new worktree"));
        // Single-plane form: every section renders at once.
        assert!(frame.contains("branch  ❯"));
        assert!(frame.contains("host"));
        assert!(frame.contains("sandbox"));
        assert!(frame.contains("program"));
        assert!(frame.contains("enter create"));

        // Host-readiness badge (hosts-as-resources): rendered dim after the
        // selected env's label when the launcher provided one; absent otherwise.
        assert!(!frame.contains("✓ ready"), "no badge without a map");
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        w.set_host_badges(std::collections::HashMap::from([(
            w.host_key(),
            "✓ ready".to_string(),
        )]));
        let mut s = Surface::new(80, 24);
        w.render(&mut s, screen);
        assert!(text(&mut s).contains("✓ ready"), "badge for selected env");

        let mut cp = CreationProgress::new(1, "sz/swift-reef".into());
        cp.apply(
            CreateStep::ResolveBase,
            StepState::Done,
            Some("main".into()),
        );
        cp.apply(CreateStep::CreateWorktree, StepState::Running, None);
        cp.revealed = true;
        let mut s = Surface::new(80, 24);
        cp.render(&mut s, screen);
        let frame = text(&mut s);
        assert!(frame.contains("creating sz/swift-reef"));
        assert!(frame.contains("✓"));
        assert!(frame.contains("resolve base"));
        assert!(frame.contains("(main)"));
        assert!(frame.contains("creation continues"));

        cp.apply(
            CreateStep::CreateWorktree,
            StepState::Failed("git worktree add failed".into()),
            None,
        );
        let mut s = Surface::new(80, 24);
        cp.render(&mut s, screen);
        let frame = text(&mut s);
        assert!(frame.contains("✗"));
        assert!(frame.contains("esc dismiss"));
    }
}
