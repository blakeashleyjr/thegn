//! The new-worktree wizard (Alt+w) and its creation pipeline.
//!
//! The wizard collects every choice up front — branch name (prefilled with a
//! pregenerated adjective-noun candidate), sandbox backend, agent — while a
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Name,
    Sandbox,
    Agent,
}

/// What a key delivered to the wizard meant. `SandboxChosen` fires when the
/// user confirms the sandbox step (so the worker can start the container
/// ensure while they pick an agent); `Submit` carries the full form.
#[derive(Debug, Clone, PartialEq)]
pub enum WizardOutcome {
    Pending,
    Cancel,
    SandboxChosen(String),
    Submit(WizardChoices),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WizardChoices {
    pub name: NameChoice,
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

/// The Alt+w modal: name input (the configured branch prefix is fixed chrome,
/// the tail is editable) → sandbox list → agent list. Pure over keys; the
/// loop dispatches on [`WizardOutcome`].
#[derive(Debug)]
pub struct NewWorktreeWizard {
    pub repo_slug: String,
    prefix: String,
    step: WizardStep,
    tail: String,
    name_edited: bool,
    name_checked: bool,
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
            prefix,
            step: WizardStep::Name,
            tail,
            name_edited: false,
            name_checked: false,
            sandbox_rows,
            sandbox_sel: 0,
            agent_rows,
            agent_sel: 0,
        }
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

    fn rows(&self) -> &[(String, String)] {
        match self.step {
            WizardStep::Sandbox => &self.sandbox_rows,
            _ => &self.agent_rows,
        }
    }

    fn sel(&mut self) -> &mut usize {
        match self.step {
            WizardStep::Sandbox => &mut self.sandbox_sel,
            _ => &mut self.agent_sel,
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
        match self.step {
            WizardStep::Name => {
                match key {
                    KeyCode::Enter if !self.tail.trim().is_empty() => {
                        self.step = WizardStep::Sandbox;
                    }
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
            WizardStep::Sandbox | WizardStep::Agent => match key {
                KeyCode::DownArrow | KeyCode::Char('j') => {
                    let max = self.rows().len().saturating_sub(1);
                    let sel = self.sel();
                    *sel = (*sel + 1).min(max);
                    WizardOutcome::Pending
                }
                KeyCode::UpArrow | KeyCode::Char('k') => {
                    let sel = self.sel();
                    *sel = sel.saturating_sub(1);
                    WizardOutcome::Pending
                }
                KeyCode::Enter => {
                    if self.step == WizardStep::Sandbox {
                        let backend = self
                            .sandbox_rows
                            .get(self.sandbox_sel)
                            .map(|(k, _)| k.clone())
                            .unwrap_or_else(|| "auto".into());
                        self.step = WizardStep::Agent;
                        WizardOutcome::SandboxChosen(backend)
                    } else {
                        let sandbox = self
                            .sandbox_rows
                            .get(self.sandbox_sel)
                            .map(|(k, _)| k.clone())
                            .unwrap_or_else(|| "auto".into());
                        let agent = self
                            .agent_rows
                            .get(self.agent_sel)
                            .map(|(k, _)| k.clone())
                            .unwrap_or_else(|| "shell".into());
                        let name = if self.name_edited {
                            NameChoice::Human(self.tail.clone())
                        } else {
                            NameChoice::Generated
                        };
                        WizardOutcome::Submit(WizardChoices {
                            name,
                            sandbox,
                            agent,
                        })
                    }
                }
                _ => WizardOutcome::Pending,
            },
        }
    }

    /// Paint as a centered layer: breadcrumb, step body, footer hints.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let body_rows = match self.step {
            WizardStep::Name => 2,
            _ => self.rows().len(),
        };
        let spec = LayerSpec {
            title: format!("new worktree — {}", self.repo_slug),
            badge: Some(" Alt+w ".into()),
            cols: 54,
            rows: body_rows + 4,
            anchor: Anchor::Center,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);

        // Breadcrumb: done steps green, current accent-bold, rest faint.
        // Numbers rendered as padded chip boxes matching the tab-bar style.
        let crumb = |step: WizardStep, n: &str, label: &str| -> Vec<Seg> {
            let done = (self.step == WizardStep::Sandbox && step == WizardStep::Name)
                || (self.step == WizardStep::Agent && step != WizardStep::Agent);
            let (chip, text) = if done {
                (
                    Seg::chip(Tok::Hue(Hue::Green), format!(" {n} ")),
                    seg(Tok::Hue(Hue::Green), label.to_string()),
                )
            } else if self.step == step {
                (
                    Seg::chip(Tok::Slot(S::Accent), format!(" {n} ")),
                    seg(Tok::Slot(S::Accent), label.to_string()).bold(),
                )
            } else {
                (
                    Seg::chip(Tok::Slot(S::Faint), format!(" {n} ")),
                    seg(Tok::Slot(S::Faint), label.to_string()),
                )
            };
            vec![
                chip,
                sp(1),
                text,
                seg(Tok::Slot(S::Faint), "  ›  ".to_string()),
            ]
        };
        let mut crumbs: Vec<Seg> = Vec::new();
        crumbs.extend(crumb(WizardStep::Name, "1", "name"));
        crumbs.extend(crumb(WizardStep::Sandbox, "2", "sandbox"));
        crumbs.extend(crumb(WizardStep::Agent, "3", "agent"));
        crumbs.pop(); // trailing separator
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(crumbs),
            panel,
        );

        let body_y = inner.y + 2;
        match self.step {
            WizardStep::Name => {
                seg::draw_line(
                    surface,
                    inner.x,
                    body_y,
                    inner.cols,
                    &Line::segs(vec![
                        seg(Tok::Slot(S::Accent), "branch ❯ ").bold(),
                        seg(Tok::Slot(S::Faint), self.prefix.clone()),
                        seg(Tok::Slot(S::Text), self.tail.clone()),
                        seg(Tok::Slot(S::Accent), "▏"),
                    ]),
                    panel,
                );
                if !self.name_checked && !self.name_edited {
                    seg::draw_line(
                        surface,
                        inner.x,
                        body_y + 1,
                        inner.cols,
                        &Line::segs(vec![
                            sp(9),
                            seg(Tok::Slot(S::Faint), "checking collisions…".to_string()),
                        ]),
                        panel,
                    );
                }
            }
            WizardStep::Sandbox | WizardStep::Agent => {
                let selected_row = match self.step {
                    WizardStep::Sandbox => self.sandbox_sel,
                    _ => self.agent_sel,
                };
                for (row, (_, label)) in self.rows().iter().enumerate().take(inner.rows - 4) {
                    let selected = row == selected_row;
                    let pad = if selected { Tok::SelAccent } else { panel };
                    let marker = if selected {
                        seg(Tok::Slot(S::Accent), "❯ ").bold()
                    } else {
                        sp(2)
                    };
                    let label_fg = if selected {
                        Tok::Slot(S::Text)
                    } else {
                        Tok::Slot(S::Dim)
                    };
                    let mut l = seg(label_fg, label.clone());
                    if selected {
                        l = l.bold();
                    }
                    seg::draw_line(
                        surface,
                        inner.x,
                        body_y + row,
                        inner.cols,
                        &Line::segs(vec![marker, l]),
                        pad,
                    );
                }
            }
        }

        let footer = match self.step {
            WizardStep::Name => "enter accept · type to rename · esc cancel",
            WizardStep::Sandbox => "enter choose · j/k move · esc back",
            WizardStep::Agent => "enter create · j/k move · esc back",
        };
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows - 1,
            inner.cols,
            &Line::segs(vec![seg(Tok::Slot(S::Faint), footer.to_string())]),
            panel,
        );
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
    SandboxChosen(String),
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
/// sandbox prep on `SandboxChosen`, rename/register/compose on `Submit`,
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
    let mut prepped: Option<(String, crate::agent::SandboxOutcome)> = None;
    let prep = |backend: &str, wt: &Path| -> anyhow::Result<crate::agent::SandboxOutcome> {
        let wt_s = wt.to_string_lossy();
        let loc = GitLoc::from_db(&wt_s, None);
        // The wizard passes the user's fresh, explicit pick — it must win over
        // the configured default backend, so `choice_is_explicit = true`.
        crate::agent::prepare_sandbox(cfg, root, &wt_s, &loc, Some(backend), true)
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
            Ok(WizardCmd::SandboxChosen(backend)) => {
                if prepped.as_ref().map(|(b, _)| b.as_str()) == Some(backend.as_str()) {
                    continue; // re-entered the step, same choice
                }
                step(CreateStep::SandboxPrep, StepState::Running, None);
                match prep(&backend, &path) {
                    Ok(outcome) => {
                        step(
                            CreateStep::SandboxPrep,
                            StepState::Done,
                            Some(sandbox_detail(&outcome)),
                        );
                        prepped = Some((backend, outcome));
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
                        if let Some((backend, outcome)) = prepped.as_mut()
                            && outcome.spec.is_some()
                        {
                            let backend = backend.clone();
                            step(CreateStep::SandboxPrep, StepState::Running, None);
                            match prep(&backend, &path) {
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

    // Submit without a sandbox prep (defensive — the wizard always emits
    // SandboxChosen before Submit): prepare now with the submitted choice.
    let (_, sandbox) = match prepped {
        Some(p) => p,
        None => {
            step(CreateStep::SandboxPrep, StepState::Running, None);
            match prep(&choices.sandbox, &path) {
                Ok(outcome) => {
                    step(
                        CreateStep::SandboxPrep,
                        StepState::Done,
                        Some(sandbox_detail(&outcome)),
                    );
                    (choices.sandbox.clone(), outcome)
                }
                Err(e) => {
                    worktree::remove(root, &path, &branch, true);
                    fail(CreateStep::SandboxPrep, e.to_string());
                    return;
                }
            }
        }
    };

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
            if let Err(e) = db.put_worktree(&tab, &root_s, &path_s, &branch, None) {
                fail(CreateStep::Register, format!("db: {e}"));
                return;
            }
            let _ = db.set_worktree_sandbox(&path_s, &sandbox.backend_label);
            let _ = db.set_worktree_agent(&path_s, &choices.agent);
            step(CreateStep::Register, StepState::Done, None);
        }
        Err(e) => {
            fail(CreateStep::Register, format!("db: {e}"));
            return;
        }
    }

    // --- compose the launch spec (pure); the loop does the openpty+exec.
    let loc = GitLoc::from_db(&path_s, None);
    let spec =
        crate::agent::compose_spec(cfg, &path_s, Some(&branch), &choices.agent, &loc, &sandbox);
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
        let cfg = test_cfg();
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

        assert_eq!(key(&mut w, KeyCode::Enter), WizardOutcome::Pending); // → sandbox
        let chosen = key(&mut w, KeyCode::Enter); // → agent, emits the choice
        let WizardOutcome::SandboxChosen(backend) = chosen else {
            panic!("expected SandboxChosen, got {chosen:?}");
        };
        let submit = key(&mut w, KeyCode::Enter);
        let WizardOutcome::Submit(choices) = submit else {
            panic!("expected Submit, got {submit:?}");
        };
        assert_eq!(choices.name, NameChoice::Generated);
        assert_eq!(choices.sandbox, backend);
        assert!(!choices.agent.is_empty());
    }

    #[test]
    fn typing_marks_edited_and_blocks_suggestion() {
        let cfg = test_cfg();
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        key(&mut w, KeyCode::Char('x'));
        let typed = w.candidate();
        w.apply_name_suggestion("sz/other-name");
        assert_eq!(w.candidate(), typed, "typed name never clobbered");

        key(&mut w, KeyCode::Enter);
        key(&mut w, KeyCode::Enter);
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
        for _ in 0..64 {
            key(&mut w, KeyCode::Backspace);
        }
        assert_eq!(key(&mut w, KeyCode::Enter), WizardOutcome::Pending);
        // Still on the name step: typing edits, Esc cancels.
        assert_eq!(key(&mut w, KeyCode::Escape), WizardOutcome::Cancel);
    }

    #[test]
    fn esc_cancels_from_any_step_and_ctrl_c_cancels_anywhere() {
        let cfg = test_cfg();
        // Escape cancels immediately from the name step.
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        assert_eq!(key(&mut w, KeyCode::Escape), WizardOutcome::Cancel);

        // Escape cancels immediately from the sandbox step.
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        key(&mut w, KeyCode::Enter); // name → sandbox
        assert_eq!(key(&mut w, KeyCode::Escape), WizardOutcome::Cancel);

        // Escape cancels immediately from the agent step.
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        key(&mut w, KeyCode::Enter); // name → sandbox
        key(&mut w, KeyCode::Enter); // sandbox → agent
        assert_eq!(key(&mut w, KeyCode::Escape), WizardOutcome::Cancel);

        // Ctrl+c cancels from any step.
        let mut w = NewWorktreeWizard::new(std::env::temp_dir(), &cfg);
        key(&mut w, KeyCode::Enter);
        assert_eq!(
            w.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            WizardOutcome::Cancel
        );
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
                WizardCmd::SandboxChosen("host".into()),
                WizardCmd::Submit(WizardChoices {
                    name: NameChoice::Generated,
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
                WizardCmd::SandboxChosen("host".into()),
                WizardCmd::Submit(WizardChoices {
                    name: NameChoice::Human("My Fix".into()),
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
        // Step numbers now render as padded chip boxes; the text and number
        // are separate segments so assert them individually.
        assert!(frame.contains(" 1 "));
        assert!(frame.contains("name"));
        assert!(frame.contains("branch ❯"));
        assert!(frame.contains("esc cancel"));

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
