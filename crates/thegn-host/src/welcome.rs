//! The first-run welcome tour (openspec `add-first-run-tour`).
//!
//! A full-screen, eight-chapter walk through thegn: the workspace model, a
//! live theme preview, an honest account of the sandbox tiers (with the
//! backends that actually work on this machine), a default coding agent,
//! remote offload, a key drill on the user's real chords, and a finish screen
//! that shows the exact config diff before anything is written.
//!
//! Pure over its inputs, like [`crate::onboarding::OnboardingWizard`]:
//! `handle_key` returns a [`TourOutcome`]; probes, config writes and the
//! palette preview are loop-side glue in [`crate::handlers::onboarding`].
//! Nothing animates — the tour is static by design (reduced motion, e2e
//! determinism), and no state is carried by colour alone: every status has a
//! word and a glyph that degrades through `caps::active_glyphs()`.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;
use unicode_width::UnicodeWidthStr;

use crate::caps::glyph;
use crate::chrome::{self, S, col};
use crate::compositor::Rect;
use crate::onboarding::{AgentInfo, ProbeRequest, ProbeResult, WriteOp};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use crate::sequence::Key;
use thegn_core::config::Config;
use thegn_core::sandbox_support::{BackendState, BackendSupport};
use thegn_core::termcaps::Glyph;
use thegn_core::theme::Hue;

/// The tour's chapters, in order. One screen each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chapter {
    Welcome,
    Workspace,
    Look,
    Sandbox,
    Agents,
    Remote,
    Keys,
    Finish,
}

pub const CHAPTERS: &[Chapter] = &[
    Chapter::Welcome,
    Chapter::Workspace,
    Chapter::Look,
    Chapter::Sandbox,
    Chapter::Agents,
    Chapter::Remote,
    Chapter::Keys,
    Chapter::Finish,
];

/// The tiers offered as the default, weakest first (`config::SandboxProfile`
/// spellings). A configured tier outside this list (`sealed-tunnel`) is kept
/// as an extra row so a re-run never silently downgrades it.
const TIERS: &[&str] = &["open", "standard", "sealed"];
const KEYMAP_PRESETS: &[&str] = &["default", "vscode", "jetbrains"];
/// Below this the tour shows a one-screen notice instead of the chapters.
const MIN_COLS: usize = 60;
const MIN_ROWS: usize = 18;
/// Widest the content column grows.
const MAX_WIDTH: usize = 78;
/// Wrong presses on one drill before the tour names the likely cause.
const MISS_HINT_AFTER: u8 = 3;

/// Side effects for the loop. The tour stays pure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TourEffects {
    /// Live-apply this theme preset to the runtime palette (not persisted).
    pub preview_theme: Option<String>,
    /// Off-loop probes to start.
    pub probes: Vec<ProbeRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TourOutcome {
    Pending,
    Do(TourEffects),
    /// Close the tour. `completed` = left from the finish screen; `writes`
    /// are applied (empty when dismissed); `open_setup` hands off to the
    /// classic setup wizard.
    Close {
        completed: bool,
        writes: Vec<WriteOp>,
        open_setup: bool,
    },
}

/// One row of the finish screen's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub key: &'static str,
    pub old: String,
    pub new: String,
    pub op: WriteOp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe<T> {
    Idle,
    Pending,
    Done(T),
}

/// The parts of the workspace diagram the Workspace chapter walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Project,
    Worktree,
    Tab,
    Pane,
}

const PARTS: &[Part] = &[Part::Project, Part::Worktree, Part::Tab, Part::Pane];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrillKind {
    NewWorktree,
    Split,
    Palette,
    Close,
}

/// One practice chord. `key` is `None` when the action has no single-key
/// chord (unbound, or a multi-key sequence) — it is shown, not drilled.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Drill {
    kind: DrillKind,
    label: &'static str,
    chord: String,
    key: Option<Key>,
    done: bool,
}

impl Drill {
    fn new(kind: DrillKind, label: &'static str, chord: &str) -> Self {
        Drill {
            kind,
            label,
            chord: chord.to_string(),
            key: single_key(chord),
            done: false,
        }
    }
}

/// Position of `v` in `list`, or 0.
fn index_of(list: &[String], v: &str) -> usize {
    list.iter().position(|x| x == v).unwrap_or(0)
}

/// The practice workspace: what the diagram draws and the drill mutates.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mini {
    /// (worktree name, pane count) — the active one's panes are drawn.
    worktrees: Vec<(String, usize)>,
    active: usize,
    palette: bool,
}

const MAX_MINI_WORKTREES: usize = 3;
const MAX_MINI_PANES: usize = 3;
const SPARE_NAMES: &[&str] = &["fix-login", "try-idea", "spike"];

impl Mini {
    fn single() -> Self {
        Mini {
            worktrees: vec![("main".into(), 1)],
            active: 0,
            palette: false,
        }
    }

    fn sample() -> Self {
        Mini {
            worktrees: vec![("main".into(), 2), ("fix-login".into(), 1)],
            active: 0,
            palette: false,
        }
    }

    fn apply(&mut self, kind: DrillKind) {
        match kind {
            DrillKind::NewWorktree => {
                if self.worktrees.len() < MAX_MINI_WORKTREES {
                    let name = SPARE_NAMES
                        .iter()
                        .find(|n| !self.worktrees.iter().any(|(w, _)| w == *n))
                        .copied()
                        .unwrap_or("spike");
                    self.worktrees.push((name.to_string(), 1));
                }
                self.active = self.worktrees.len() - 1;
            }
            DrillKind::Split => {
                if let Some((_, panes)) = self.worktrees.get_mut(self.active) {
                    *panes = (*panes + 1).min(MAX_MINI_PANES);
                }
            }
            DrillKind::Palette => self.palette = true,
            DrillKind::Close => {
                self.palette = false;
                if let Some((_, panes)) = self.worktrees.get_mut(self.active)
                    && *panes > 1
                {
                    *panes -= 1;
                }
            }
        }
    }
}

/// Live chord hints, resolved once from the effective keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hints {
    new_worktree: String,
    new_project: String,
    new_pane: String,
    palette: String,
    close: String,
    help: String,
}

fn hint(cfg: &Config, id: &str, fallback: &str) -> String {
    crate::keymap::chord_hint_for(cfg, id).unwrap_or_else(|| fallback.to_string())
}

/// The single key a display chord (`"Alt-w"`, `"Ctrl-Space"`) is pressed as;
/// `None` for an unparsable or multi-key chord.
fn single_key(chord: &str) -> Option<Key> {
    let mut keys = crate::keymap::parse_chord(chord).ok()?;
    (keys.len() == 1).then(|| keys.remove(0))
}

/// A pressed key in the display form the hints use (`Alt-q`, `Ctrl-Space`).
fn describe(key: &Key) -> String {
    let mut out = String::new();
    for (m, name) in [
        (Modifiers::CTRL, "Ctrl-"),
        (Modifiers::ALT, "Alt-"),
        (Modifiers::SUPER, "Super-"),
        (Modifiers::SHIFT, "Shift-"),
    ] {
        if key.mods.contains(m) {
            out.push_str(name);
        }
    }
    let code = match key.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) if c.is_control() => format!("0x{:02x}", c as u32),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::LeftArrow => "Left".into(),
        KeyCode::RightArrow => "Right".into(),
        KeyCode::UpArrow => "Up".into(),
        KeyCode::DownArrow => "Down".into(),
        KeyCode::Function(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };
    out.push_str(&code);
    out
}

pub struct WelcomeTour {
    chapter: Chapter,
    hints: Hints,
    config_path: String,
    // Workspace
    part: usize,
    // Look
    themes: Vec<String>,
    init_theme: String,
    theme_sel: usize,
    init_keymap: String,
    keymap_sel: usize,
    // Sandbox
    sandbox_enabled: bool,
    tiers: Vec<String>,
    init_tier: String,
    tier_sel: usize,
    backends: Vec<String>,
    init_backend: String,
    backend_sel: usize,
    sandbox: Probe<Vec<BackendSupport>>,
    // Agents
    agents: Probe<Vec<AgentInfo>>,
    init_agent: Option<String>,
    agent_init_sel: Option<usize>,
    agent_sel: Option<usize>,
    agent_keep: Vec<(String, String)>,
    agent_note: Option<String>,
    // Keys
    drills: Vec<Drill>,
    drill_at: usize,
    misses: u8,
    feedback: Option<(bool, String)>,
    mini: Mini,
    /// Both probes are kicked on the first step past Welcome.
    probes_started: bool,
}

impl WelcomeTour {
    pub fn new(cfg: &Config) -> Self {
        let init_theme = if cfg.theme.preset.is_empty() {
            "prism".to_string()
        } else {
            cfg.theme.preset.clone()
        };
        let mut themes: Vec<String> = thegn_core::theme::PRESETS
            .iter()
            .map(|p| p.to_string())
            .collect();
        if !themes.contains(&init_theme) {
            // A user theme or custom preset: keep it selectable (and selected).
            themes.insert(0, init_theme.clone());
        }
        let init_keymap = if cfg.keymap_preset.is_empty() {
            "default".to_string()
        } else {
            cfg.keymap_preset.clone()
        };
        let init_tier = cfg.sandbox.profile.as_str().to_string();
        let mut tiers: Vec<String> = TIERS.iter().map(|t| t.to_string()).collect();
        if !tiers.contains(&init_tier) {
            tiers.push(init_tier.clone());
        }
        let init_backend = cfg.sandbox.backend.as_str().to_string();
        let mut backends = vec!["auto".to_string()];
        for b in std::iter::once(&init_backend).chain(cfg.sandbox.backend_chain.iter()) {
            if !backends.contains(b) {
                backends.push(b.clone());
            }
        }
        let hints = Hints {
            new_worktree: hint(cfg, "new-worktree", "Alt-w"),
            new_project: hint(cfg, "new-project", "Alt-W"),
            new_pane: hint(cfg, "new-pane", "Alt-p"),
            palette: hint(cfg, "palette", "Ctrl-Space"),
            close: hint(cfg, "close", "Alt-x"),
            help: hint(cfg, "help", "F1"),
        };
        let drills = vec![
            Drill::new(DrillKind::NewWorktree, "new worktree", &hints.new_worktree),
            Drill::new(DrillKind::Split, "split the pane", &hints.new_pane),
            Drill::new(DrillKind::Palette, "command palette", &hints.palette),
            Drill::new(DrillKind::Close, "close a pane", &hints.close),
        ];
        WelcomeTour {
            chapter: Chapter::Welcome,
            config_path: Config::path().display().to_string(),
            part: 0,
            theme_sel: index_of(&themes, &init_theme),
            themes,
            init_theme,
            keymap_sel: KEYMAP_PRESETS
                .iter()
                .position(|k| *k == init_keymap)
                .unwrap_or(0),
            init_keymap,
            sandbox_enabled: cfg.sandbox.enabled,
            tier_sel: index_of(&tiers, &init_tier),
            tiers,
            init_tier,
            backend_sel: index_of(&backends, &init_backend),
            backends,
            init_backend,
            sandbox: Probe::Idle,
            agents: Probe::Idle,
            init_agent: cfg.default_agent_name().map(str::to_string),
            agent_init_sel: None,
            agent_sel: None,
            agent_keep: cfg
                .agents
                .iter()
                .map(|a| (a.name.clone(), a.command.clone()))
                .collect(),
            agent_note: None,
            drills,
            drill_at: 0,
            misses: 0,
            feedback: None,
            mini: Mini::single(),
            probes_started: false,
            hints,
        }
    }

    // ---- probes ------------------------------------------------------------

    fn sandbox_chain(&self) -> Vec<String> {
        self.backends
            .iter()
            .filter(|b| *b != "auto")
            .cloned()
            .collect()
    }

    /// Deliver an off-thread probe answer.
    pub fn apply_probe(&mut self, result: ProbeResult) {
        match result {
            ProbeResult::Sandbox(rows) => self.sandbox = Probe::Done(rows),
            ProbeResult::Agents(list) => {
                let current = self
                    .init_agent
                    .as_deref()
                    .and_then(|cur| list.iter().position(|a| a.id == cur));
                let sel = current.or_else(|| list.iter().position(|a| a.path.is_some()));
                self.agent_init_sel = sel;
                if self.agent_sel.is_none() {
                    self.agent_sel = sel;
                }
                self.agents = Probe::Done(list);
            }
            ProbeResult::Forge(_) | ProbeResult::Host { .. } => {}
        }
    }

    /// The probes still to run, marking them pending.
    fn start_probes(&mut self) -> Vec<ProbeRequest> {
        let mut out = Vec::new();
        if self.sandbox == Probe::Idle {
            self.sandbox = Probe::Pending;
            out.push(ProbeRequest::Sandbox(self.sandbox_chain()));
        }
        if self.agents == Probe::Idle {
            self.agents = Probe::Pending;
            out.push(ProbeRequest::Agents);
        }
        self.probes_started = true;
        out
    }

    // ---- flow --------------------------------------------------------------

    fn index(&self) -> usize {
        CHAPTERS
            .iter()
            .position(|c| *c == self.chapter)
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn chapter(&self) -> Chapter {
        self.chapter
    }

    fn go(&mut self, chapter: Chapter) -> TourOutcome {
        self.chapter = chapter;
        self.agent_note = None;
        if chapter == Chapter::Keys {
            // Every visit is a fresh practice run.
            self.mini = Mini::single();
            self.misses = 0;
            self.feedback = None;
            for d in &mut self.drills {
                d.done = false;
            }
            self.drill_at = self
                .drills
                .iter()
                .position(|d| d.key.is_some())
                .unwrap_or(self.drills.len());
        }
        let probes = if self.probes_started || chapter != Chapter::Welcome {
            self.start_probes()
        } else {
            Vec::new()
        };
        TourOutcome::Do(TourEffects {
            probes,
            ..TourEffects::default()
        })
    }

    fn next(&mut self) -> TourOutcome {
        match CHAPTERS.get(self.index() + 1) {
            Some(next) => self.go(*next),
            None => self.finish(false),
        }
    }

    fn back(&mut self) -> TourOutcome {
        match self.index().checked_sub(1).and_then(|i| CHAPTERS.get(i)) {
            Some(prev) => self.go(*prev),
            None => TourOutcome::Pending,
        }
    }

    /// Tab: drop this chapter's choice (back to the configured value) and move on.
    fn skip(&mut self) -> TourOutcome {
        let mut revert = None;
        match self.chapter {
            Chapter::Look => {
                let was = self.themes[self.theme_sel].clone();
                self.theme_sel = self
                    .themes
                    .iter()
                    .position(|t| *t == self.init_theme)
                    .unwrap_or(0);
                self.keymap_sel = KEYMAP_PRESETS
                    .iter()
                    .position(|k| *k == self.init_keymap)
                    .unwrap_or(0);
                if was != self.init_theme {
                    revert = Some(self.init_theme.clone());
                }
            }
            Chapter::Sandbox => {
                self.tier_sel = self
                    .tiers
                    .iter()
                    .position(|t| *t == self.init_tier)
                    .unwrap_or(0);
                self.backend_sel = self
                    .backends
                    .iter()
                    .position(|b| *b == self.init_backend)
                    .unwrap_or(0);
            }
            Chapter::Agents => self.agent_sel = self.agent_init_sel,
            _ => {}
        }
        match self.next() {
            TourOutcome::Do(mut fx) => {
                fx.preview_theme = revert;
                TourOutcome::Do(fx)
            }
            other => other,
        }
    }

    fn finish(&mut self, open_setup: bool) -> TourOutcome {
        TourOutcome::Close {
            completed: true,
            writes: self.changes().into_iter().map(|c| c.op).collect(),
            open_setup,
        }
    }

    fn dismiss(open_setup: bool) -> TourOutcome {
        TourOutcome::Close {
            completed: false,
            writes: Vec::new(),
            open_setup,
        }
    }

    /// Everything the finish screen would write, in display order. A choice
    /// equal to the configured value writes nothing.
    pub fn changes(&self) -> Vec<Change> {
        let mut out = Vec::new();
        let theme = &self.themes[self.theme_sel];
        if *theme != self.init_theme {
            out.push(Change {
                key: "theme.preset",
                old: self.init_theme.clone(),
                new: theme.clone(),
                op: WriteOp::Set {
                    key: "theme.preset".into(),
                    value: theme.clone(),
                },
            });
        }
        let keymap = KEYMAP_PRESETS[self.keymap_sel];
        if keymap != self.init_keymap {
            out.push(Change {
                key: "keymap preset",
                old: self.init_keymap.clone(),
                new: keymap.to_string(),
                op: WriteOp::KeymapPreset(keymap.to_string()),
            });
        }
        let tier = &self.tiers[self.tier_sel];
        if *tier != self.init_tier {
            out.push(Change {
                key: "sandbox.profile",
                old: self.init_tier.clone(),
                new: tier.clone(),
                op: WriteOp::Set {
                    key: "sandbox.profile".into(),
                    value: tier.clone(),
                },
            });
        }
        let backend = &self.backends[self.backend_sel];
        if *backend != self.init_backend {
            out.push(Change {
                key: "sandbox.backend",
                old: self.init_backend.clone(),
                new: backend.clone(),
                op: WriteOp::Set {
                    key: "sandbox.backend".into(),
                    value: backend.clone(),
                },
            });
        }
        if let (Probe::Done(list), Some(i)) = (&self.agents, self.agent_sel)
            && let Some(a) = list.get(i)
            && a.path.is_some()
            && self.agent_sel != self.agent_init_sel
            && self.init_agent.as_deref() != Some(a.id.as_str())
        {
            out.push(Change {
                key: "default agent",
                old: self.init_agent.clone().unwrap_or_else(|| "shell".into()),
                new: a.id.clone(),
                op: WriteOp::PromoteAgent {
                    name: a.id.clone(),
                    command: a.command.clone(),
                    keep: self.agent_keep.clone(),
                },
            });
        }
        out
    }

    fn preview(&self) -> TourOutcome {
        TourOutcome::Do(TourEffects {
            preview_theme: Some(self.themes[self.theme_sel].clone()),
            ..TourEffects::default()
        })
    }

    // ---- input -------------------------------------------------------------

    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> TourOutcome {
        let shift = mods.contains(Modifiers::SHIFT);
        // Navigation keys work everywhere, including the drill.
        match key {
            k if crate::input::is_escape_key(k) => return Self::dismiss(false),
            KeyCode::Char('c' | 'C') if mods == Modifiers::CTRL => return Self::dismiss(false),
            KeyCode::Tab if shift => return self.back(),
            KeyCode::Tab => return self.skip(),
            KeyCode::Backspace if mods.is_empty() => return self.back(),
            KeyCode::Enter if mods.is_empty() => return self.enter(),
            _ => {}
        }
        if self.chapter == Chapter::Keys {
            return self.drill_key(key, mods);
        }
        if mods.intersects(Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER) {
            return TourOutcome::Pending;
        }
        let delta = match key {
            KeyCode::UpArrow | KeyCode::Char('k') => -1,
            KeyCode::DownArrow | KeyCode::Char('j') => 1,
            _ => 0,
        };
        match (self.chapter, key) {
            (Chapter::Welcome, KeyCode::Char('a')) => Self::dismiss(true),
            (Chapter::Finish, KeyCode::Char('a')) => self.finish(true),
            (Chapter::Workspace, _) if delta != 0 => {
                self.part = step(self.part, delta, PARTS.len(), false);
                TourOutcome::Pending
            }
            (Chapter::Look, _) if delta != 0 => {
                self.theme_sel = step(self.theme_sel, delta, self.themes.len(), true);
                self.preview()
            }
            (Chapter::Look, KeyCode::LeftArrow | KeyCode::RightArrow) => {
                let d = if *key == KeyCode::LeftArrow { -1 } else { 1 };
                self.keymap_sel = step(self.keymap_sel, d, KEYMAP_PRESETS.len(), true);
                TourOutcome::Pending
            }
            (Chapter::Sandbox, _) if delta != 0 => {
                self.tier_sel = step(self.tier_sel, delta, self.tiers.len(), false);
                TourOutcome::Pending
            }
            (Chapter::Sandbox, KeyCode::LeftArrow | KeyCode::RightArrow) => {
                let d = if *key == KeyCode::LeftArrow { -1 } else { 1 };
                self.backend_sel = step(self.backend_sel, d, self.backends.len(), true);
                TourOutcome::Pending
            }
            // `r` re-checks the runtimes after the user starts one; the probe
            // cache would otherwise re-serve the stale negative answer.
            (Chapter::Sandbox, KeyCode::Char('r' | 'R')) if self.sandbox != Probe::Pending => {
                thegn_core::sandbox_backend::clear_probe_cache();
                self.sandbox = Probe::Pending;
                TourOutcome::Do(TourEffects {
                    probes: vec![ProbeRequest::Sandbox(self.sandbox_chain())],
                    ..TourEffects::default()
                })
            }
            (Chapter::Agents, _) if delta != 0 => {
                if let Probe::Done(list) = &self.agents
                    && !list.is_empty()
                {
                    let cur = self.agent_sel.unwrap_or(0);
                    self.agent_sel = Some(step(cur, delta, list.len(), false));
                    self.agent_note = None;
                }
                TourOutcome::Pending
            }
            _ => TourOutcome::Pending,
        }
    }

    fn enter(&mut self) -> TourOutcome {
        match self.chapter {
            Chapter::Finish => self.finish(false),
            Chapter::Agents => {
                if let (Probe::Done(list), Some(i)) = (&self.agents, self.agent_sel)
                    && self.agent_sel != self.agent_init_sel
                    && let Some(a) = list.get(i)
                    && a.path.is_none()
                {
                    self.agent_note = Some(format!(
                        "{} is not installed here, so it can't be the default. \
                         Pick an installed one, or Tab to skip.",
                        a.name
                    ));
                    return TourOutcome::Pending;
                }
                self.next()
            }
            _ => self.next(),
        }
    }

    fn drill_key(&mut self, key: &KeyCode, mods: Modifiers) -> TourOutcome {
        let Some(drill) = self.drills.get(self.drill_at) else {
            return TourOutcome::Pending; // all done — only Enter/Tab/Esc matter
        };
        let pressed = Key::modified(*key, mods);
        if drill.key.as_ref() == Some(&pressed) {
            let kind = drill.kind;
            self.drills[self.drill_at].done = true;
            self.mini.apply(kind);
            self.misses = 0;
            self.feedback = Some((true, self.drill_success(kind)));
            self.drill_at += 1;
            // An undrillable chord (unbound / multi-key) is skipped over.
            while self
                .drills
                .get(self.drill_at)
                .is_some_and(|d| d.key.is_none())
            {
                self.drill_at += 1;
            }
            return TourOutcome::Pending;
        }
        if drill.key.is_none() {
            self.drill_at += 1;
            return TourOutcome::Pending;
        }
        self.misses = self.misses.saturating_add(1);
        let msg = if self.misses >= MISS_HINT_AFTER {
            format!(
                "Still nothing as {}? Your terminal may not be sending it (on macOS, set \
                 Option to act as Alt). Help: terminal-compatibility. Tab skips.",
                drill.chord
            )
        } else {
            format!("That was {} — try {}.", describe(&pressed), drill.chord)
        };
        self.feedback = Some((false, msg));
        TourOutcome::Pending
    }

    fn drill_success(&self, kind: DrillKind) -> String {
        match kind {
            DrillKind::NewWorktree => {
                "A new worktree: its own branch, its own folder, its own tab.".into()
            }
            DrillKind::Split => format!("Split. {} closes it again.", self.hints.close),
            DrillKind::Palette => {
                "The palette: every action, fuzzy-searched. Esc closes it.".into()
            }
            DrillKind::Close => {
                "Closed. On the last pane it closes the tab; with Shift, the worktree.".into()
            }
        }
    }

    // ---- render ------------------------------------------------------------

    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        chrome::fill(surface, screen, col(S::Bg0));
        let bg = Tok::Slot(S::Bg0);
        if screen.cols < MIN_COLS || screen.rows < MIN_ROWS {
            let lines = [
                Line::segs(vec![seg(Tok::Slot(S::Accent), "thegn welcome tour").bold()]),
                Line::segs(vec![seg(
                    Tok::Slot(S::Text),
                    format!("needs {MIN_COLS}x{MIN_ROWS}; enlarge the window,"),
                )]),
                Line::segs(vec![seg(
                    Tok::Slot(S::Text),
                    "or press Esc to skip it.".to_string(),
                )]),
            ];
            for (i, line) in lines.iter().enumerate().take(screen.rows) {
                seg::draw_line(surface, screen.x, screen.y + i, screen.cols, line, bg);
            }
            return;
        }
        let width = screen.cols.saturating_sub(4).min(MAX_WIDTH);
        let x = screen.x + (screen.cols - width) / 2;
        seg::draw_line(surface, x, screen.y, width, &self.header(width), bg);
        // The gap row under the header carries the skip control on every
        // chapter, so the way out is named on the screen itself and never only
        // in the footer's list of chords.
        seg::draw_line(surface, x, screen.y + 1, width, &Self::skip_line(width), bg);
        let footer_y = screen.y + screen.rows - 1;
        seg::draw_line(surface, x, footer_y, width, &self.footer(), bg);
        let mut y = screen.y + 2;
        let body_end = footer_y.saturating_sub(1);
        if self.chapter == Chapter::Welcome {
            y += self.draw_wordmark(surface, x, y, width);
        }
        for line in self.body_lines(width) {
            if y >= body_end {
                break;
            }
            seg::draw_line(surface, x, y, width, &line, bg);
            y += 1;
        }
    }

    /// The pixel wordmark on a Unicode terminal; plain text on an ASCII one.
    /// Returns the rows used (wordmark + one blank).
    fn draw_wordmark(&self, surface: &mut Surface, x: usize, y: usize, width: usize) -> usize {
        use crate::logotype::{Face, WORDMARK, draw, measure};
        let pixel_ok = crate::caps::unicode_level() != thegn_core::termcaps::UnicodeLevel::Ascii;
        let (cols, rows) = measure(Face::Large, WORDMARK);
        if pixel_ok && cols <= width {
            draw(
                surface,
                x,
                y,
                WORDMARK,
                Face::Large,
                col(S::Accent),
                col(S::Bg0),
                width,
                rows,
            );
            rows + 1
        } else {
            let line = Line::segs(vec![seg(Tok::Slot(S::Accent), "T H E G N").bold()]);
            seg::draw_line(surface, x, y, width, &line, Tok::Slot(S::Bg0));
            2
        }
    }

    fn title(&self) -> &'static str {
        match self.chapter {
            Chapter::Welcome => "welcome",
            Chapter::Workspace => "the workspace model",
            Chapter::Look => "pick your look",
            Chapter::Sandbox => "sandboxes, honestly",
            Chapter::Agents => "your coding agent",
            Chapter::Remote => "remote offload",
            Chapter::Keys => "keys you'll use daily",
            Chapter::Finish => "finish",
        }
    }

    fn header(&self, width: usize) -> Line {
        let left = vec![
            seg(Tok::Slot(S::Accent), "thegn").bold(),
            seg(Tok::Slot(S::Faint), format!(" {} ", glyph(Glyph::Middot))),
            seg(Tok::Slot(S::Text), self.title().to_string()).bold(),
        ];
        let mut dots: Vec<Seg> = Vec::new();
        let at = self.index();
        for i in 0..CHAPTERS.len() {
            if i > 0 {
                dots.push(sp(1));
            }
            dots.push(if i == at {
                seg(Tok::Slot(S::Accent), glyph(Glyph::DotFilled)).bold()
            } else if i < at {
                seg(Tok::Slot(S::Text), glyph(Glyph::DotFilled))
            } else {
                seg(Tok::Slot(S::Faint), glyph(Glyph::DotHollow))
            });
        }
        dots.push(seg(
            Tok::Slot(S::Faint),
            format!("  {}/{}", at + 1, CHAPTERS.len()),
        ));
        let used = segs_width(&left) + segs_width(&dots);
        let mut segs = left;
        segs.push(sp(width.saturating_sub(used)));
        segs.extend(dots);
        Line::segs(segs)
    }

    /// The persistent skip control, right-aligned under the header. It paints
    /// on every chapter (including the drill) and names the same chord the key
    /// handler honours, so "how do I get out" never has a wrong answer.
    fn skip_line(width: usize) -> Line {
        let mut segs = vec![
            seg(Tok::Slot(S::Faint), "[ ".to_string()),
            seg(Tok::Slot(S::Accent), "esc".to_string()).bold(),
            seg(Tok::Slot(S::Faint), format!(" {} ", glyph(Glyph::Middot))),
            seg(Tok::Slot(S::Text), "skip tour".to_string()),
            seg(Tok::Slot(S::Faint), " ]".to_string()),
        ];
        let used = segs_width(&segs);
        let mut line = vec![sp(width.saturating_sub(used))];
        line.append(&mut segs);
        Line::segs(line)
    }

    fn footer(&self) -> Line {
        let keys: &[(&str, &str)] = match self.chapter {
            Chapter::Welcome => &[
                ("enter", "start"),
                ("a", "classic setup"),
                ("esc", "skip tour"),
            ],
            Chapter::Workspace => &[("up/down", "explore"), ("enter", "next"), ("esc", "leave")],
            Chapter::Look => &[
                ("up/down", "theme"),
                ("left/right", "keymap"),
                ("enter", "next"),
                ("tab", "skip"),
                ("esc", "leave"),
            ],
            Chapter::Sandbox => &[
                ("up/down", "tier"),
                ("left/right", "backend"),
                ("r", "re-check"),
                ("enter", "next"),
                ("tab", "skip"),
            ],
            Chapter::Agents => &[
                ("up/down", "agent"),
                ("enter", "next"),
                ("tab", "skip"),
                ("esc", "leave"),
            ],
            Chapter::Keys => &[
                ("enter", "next"),
                ("tab", "skip"),
                ("shift-tab", "back"),
                ("esc", "leave"),
            ],
            Chapter::Finish => &[
                ("enter", "save"),
                ("a", "save + classic setup"),
                ("esc", "don't save"),
            ],
            Chapter::Remote => &[("enter", "next"), ("shift-tab", "back"), ("esc", "leave")],
        };
        let mut segs = Vec::new();
        for (i, (k, what)) in keys.iter().enumerate() {
            if i > 0 {
                segs.push(seg(
                    Tok::Slot(S::Faint),
                    format!(" {} ", glyph(Glyph::Middot)),
                ));
            }
            segs.push(seg(Tok::Slot(S::Accent), k.to_string()).bold());
            segs.push(seg(Tok::Slot(S::Faint), format!(" {what}")));
        }
        Line::segs(segs)
    }

    fn body_lines(&self, width: usize) -> Vec<Line> {
        match self.chapter {
            Chapter::Welcome => self.welcome_lines(),
            Chapter::Workspace => self.workspace_lines(width),
            Chapter::Look => self.look_lines(),
            Chapter::Sandbox => self.sandbox_lines(),
            Chapter::Agents => self.agent_lines(width),
            Chapter::Remote => self.remote_lines(),
            Chapter::Keys => self.keys_lines(width),
            Chapter::Finish => self.finish_lines(),
        }
    }

    fn welcome_lines(&self) -> Vec<Line> {
        vec![
            text("Every git worktree a tab. Every agent in its own sandbox. Nothing waits."),
            blank(),
            faint("Two minutes, seven short screens: the workspace model, your look,"),
            faint("sandboxes, agents, remote machines, the keys, and a summary."),
            blank(),
            text("Nothing is saved until the last screen, where you see the exact diff."),
            text("Tab skips a screen. Esc leaves the tour right now."),
            blank(),
            Line::segs(vec![
                seg(Tok::Slot(S::Faint), "Prefer forms? ".to_string()),
                seg(Tok::Slot(S::Accent), "a".to_string()).bold(),
                seg(
                    Tok::Slot(S::Faint),
                    " opens the classic setup (forge, issue trackers, ssh hosts).".to_string(),
                ),
            ]),
            faint("Just want a shell? Esc. It is right behind this screen."),
            faint(&format!(
                "Reopen any time: {} then \"Welcome tour\".",
                self.hints.palette
            )),
        ]
    }

    fn workspace_lines(&self, width: usize) -> Vec<Line> {
        let part = PARTS[self.part];
        let mut lines = vec![faint("up/down walks the diagram."), blank()];
        lines.extend(mini_lines(&Mini::sample(), Some(part), width));
        lines.push(blank());
        let (name, a, b) = match part {
            Part::Project => (
                "project",
                "A project is one repository, listed in the sidebar.".to_string(),
                format!("{} opens or clones one.", self.hints.new_project),
            ),
            Part::Worktree => (
                "worktree",
                "A worktree is a branch checked out in its own folder, so parallel".to_string(),
                format!(
                    "work (yours or an agent's) never collides. {} makes one.",
                    self.hints.new_worktree
                ),
            ),
            Part::Tab => (
                "tab",
                "Each worktree is a tab in the strip at the top. Switching is a tab".to_string(),
                "switch: nothing restarts, every shell keeps running.".to_string(),
            ),
            Part::Pane => (
                "pane",
                "Panes split a tab: a shell, an agent, a tool, side by side.".to_string(),
                format!(
                    "{} splits; {} closes the focused one.",
                    self.hints.new_pane, self.hints.close
                ),
            ),
        };
        lines.push(Line::segs(vec![
            seg(Tok::Slot(S::Accent), format!("{} ", glyph(Glyph::Chevron))),
            seg(Tok::Slot(S::Text), name.to_string()).bold(),
        ]));
        lines.push(text(&format!("  {a}")));
        lines.push(text(&format!("  {b}")));
        lines.push(blank());
        lines.push(faint(
            "One session: quit and relaunch, and you are back on the same pane.",
        ));
        lines
    }

    fn look_lines(&self) -> Vec<Line> {
        let mut lines = vec![
            text("The whole screen is wearing the highlighted theme right now."),
            blank(),
        ];
        // A 7-row window around the selection keeps the list inside 80x24.
        const WINDOW: usize = 7;
        let n = self.themes.len();
        let start = self
            .theme_sel
            .saturating_sub(WINDOW / 2)
            .min(n.saturating_sub(WINDOW));
        for (i, name) in self.themes.iter().enumerate().skip(start).take(WINDOW) {
            let selected = i == self.theme_sel;
            let mut segs = vec![if selected {
                seg(Tok::Slot(S::Accent), format!("{} ", glyph(Glyph::Chevron))).bold()
            } else {
                sp(2)
            }];
            segs.push(if selected {
                seg(Tok::Slot(S::Text), name.clone()).bold()
            } else {
                seg(Tok::Slot(S::Dim), name.clone())
            });
            if *name == self.init_theme {
                segs.push(seg(Tok::Slot(S::Faint), "  (current)".to_string()));
            }
            lines.push(Line::segs(segs));
        }
        lines.push(blank());
        let mut swatch = Vec::new();
        for (hue, name) in Hue::ALL.iter().zip(HUE_NAMES) {
            swatch.push(seg(Tok::Hue(*hue), format!("{} ", glyph(Glyph::DotFilled))));
            swatch.push(seg(Tok::Slot(S::Dim), format!("{name}  ")));
        }
        lines.push(Line::segs(swatch));
        lines.push(blank());
        let keymap = KEYMAP_PRESETS[self.keymap_sel];
        lines.push(Line::segs(vec![
            seg(Tok::Slot(S::Faint), "keymap  ".to_string()),
            seg(Tok::Slot(S::Accent), "< ".to_string()),
            seg(Tok::Slot(S::Text), keymap.to_string()).bold(),
            seg(Tok::Slot(S::Accent), " >".to_string()),
            seg(
                Tok::Slot(S::Faint),
                "   vscode / jetbrains add familiar chords on top".to_string(),
            ),
        ]));
        if crate::caps::color_depth() == thegn_core::termcaps::ColorDepth::None {
            lines.push(blank());
            lines.push(faint(
                "No colour here (NO_COLOR / mono terminal); the theme applies once it's on.",
            ));
        }
        lines
    }

    fn sandbox_lines(&self) -> Vec<Line> {
        let mut lines = vec![
            text("A sandbox fences what a pane can touch. Pick the tier new panes get."),
            blank(),
            Line::segs(vec![
                sp(2),
                seg(Tok::Slot(S::Faint), format!("{:<10}", "tier")),
                seg(Tok::Slot(S::Faint), format!("{:<20}", "outside worktree")),
                seg(Tok::Slot(S::Faint), format!("{:<24}", "home + credentials")),
                seg(Tok::Slot(S::Faint), "network".to_string()),
            ]),
        ];
        for (i, tier) in self.tiers.iter().enumerate() {
            let (outside, home, net) = tier_row(tier);
            let selected = i == self.tier_sel;
            let fg = if selected { S::Text } else { S::Dim };
            let mut name = seg(Tok::Slot(fg), format!("{tier:<10}"));
            if selected {
                name = name.bold();
            }
            let mut segs = vec![
                if selected {
                    seg(Tok::Slot(S::Accent), format!("{} ", glyph(Glyph::Chevron))).bold()
                } else {
                    sp(2)
                },
                name,
                seg(Tok::Slot(fg), format!("{outside:<20}")),
                seg(Tok::Slot(fg), format!("{home:<24}")),
                seg(Tok::Slot(fg), net.to_string()),
            ];
            if *tier == self.init_tier {
                segs.push(seg(Tok::Slot(S::Faint), "  (current)".to_string()));
            }
            lines.push(Line::segs(segs));
        }
        lines.push(blank());
        lines.push(text(&format!(
            "  {}",
            tier_note(&self.tiers[self.tier_sel])
        )));
        lines.push(Line::segs(vec![
            seg(Tok::Hue(Hue::Amber), format!("{} ", glyph(Glyph::Warn))),
            seg(
                Tok::Slot(S::Text),
                "No tier stops a kernel exploit (containers share the kernel), and".to_string(),
            ),
        ]));
        lines.push(text(
            "  native plugins always run unsandboxed. standard guards accidents, not malice.",
        ));
        lines.push(blank());
        lines.push(Line::segs(vec![
            seg(Tok::Slot(S::Faint), "backend  ".to_string()),
            seg(Tok::Slot(S::Accent), "< ".to_string()),
            seg(Tok::Slot(S::Text), self.backends[self.backend_sel].clone()).bold(),
            seg(Tok::Slot(S::Accent), " >".to_string()),
            seg(
                Tok::Slot(S::Faint),
                "   auto = the first ready one, in this order:".to_string(),
            ),
        ]));
        lines.extend(self.backend_status_lines());
        if !self.sandbox_enabled {
            lines.push(faint(
                "Sandboxing is off ([sandbox] enabled = false); this applies once it is on.",
            ));
        }
        lines
    }

    fn backend_status_lines(&self) -> Vec<Line> {
        const SHOWN: usize = 4;
        let rows = match &self.sandbox {
            Probe::Idle | Probe::Pending => {
                return vec![faint(&format!(
                    "  {} checking which runtimes answer on this machine",
                    glyph(Glyph::Ellipsis)
                ))];
            }
            Probe::Done(rows) => rows,
        };
        let mut lines: Vec<Line> = rows
            .iter()
            .take(SHOWN)
            .map(|r| {
                let (g, hue, word) = match r.state {
                    BackendState::Ready => (Glyph::Check, Hue::Green, "ready"),
                    BackendState::NotRunning => (Glyph::HalfDot, Hue::Amber, "not running"),
                    BackendState::NotInstalled => (Glyph::Cross, Hue::Red, "not installed"),
                    BackendState::Unsupported => (Glyph::Cross, Hue::Red, "not on this OS"),
                    BackendState::Unreachable => (Glyph::Cross, Hue::Red, "unreachable"),
                };
                let mut segs = vec![
                    sp(2),
                    seg(Tok::Hue(hue), format!("{} ", glyph(g))),
                    seg(Tok::Slot(S::Text), format!("{:<10}", r.name)),
                    seg(Tok::Slot(S::Dim), word.to_string()),
                ];
                if r.state == BackendState::NotRunning
                    && let Some(remedy) = &r.remedy
                {
                    segs.push(seg(Tok::Slot(S::Faint), format!(" — {remedy}")));
                } else if r.state.usable()
                    && let Some(caveat) = &r.caveat
                {
                    segs.push(seg(Tok::Slot(S::Faint), format!(" — {caveat}")));
                }
                Line::segs(segs)
            })
            .collect();
        if rows.len() > SHOWN {
            lines.push(faint(&format!(
                "  +{} more: thegn doctor lists them all",
                rows.len() - SHOWN
            )));
        }
        if !rows.iter().any(|r| r.state.usable()) {
            lines.push(Line::segs(vec![
                seg(Tok::Hue(Hue::Amber), format!("{} ", glyph(Glyph::Warn))),
                seg(
                    Tok::Slot(S::Text),
                    "Nothing here can sandbox a pane yet: sandboxed launches are refused"
                        .to_string(),
                ),
            ]));
            lines.push(text(
                "  until one is ready. Start or install one above, then press r.",
            ));
        }
        lines
    }

    fn agent_lines(&self, width: usize) -> Vec<Line> {
        let mut lines = vec![
            text("thegn starts coding agents in their own worktree, inside the sandbox."),
            text("Pick the one it reaches for by default."),
            blank(),
        ];
        match &self.agents {
            Probe::Idle | Probe::Pending => lines.push(faint(&format!(
                "  {} looking for agent CLIs on PATH",
                glyph(Glyph::Ellipsis)
            ))),
            Probe::Done(list) => {
                for (i, a) in list.iter().enumerate() {
                    let selected = self.agent_sel == Some(i);
                    let (g, hue, where_) = match &a.path {
                        Some(p) => (Glyph::Check, Hue::Green, p.clone()),
                        None => (Glyph::Cross, Hue::Red, "not installed".to_string()),
                    };
                    let mut segs = vec![
                        if selected {
                            seg(Tok::Slot(S::Accent), format!("{} ", glyph(Glyph::Chevron))).bold()
                        } else {
                            sp(2)
                        },
                        seg(Tok::Hue(hue), format!("{} ", glyph(g))),
                        if selected {
                            seg(Tok::Slot(S::Text), format!("{:<12}", a.name)).bold()
                        } else {
                            seg(Tok::Slot(S::Dim), format!("{:<12}", a.name))
                        },
                        seg(Tok::Slot(S::Faint), truncate(&where_, 44)),
                    ];
                    if self.init_agent.as_deref() == Some(a.id.as_str()) {
                        segs.push(seg(Tok::Slot(S::Faint), "  (current)".to_string()));
                    }
                    lines.push(Line::segs(segs));
                }
                if !list.iter().any(|a| a.path.is_some()) {
                    lines.push(blank());
                    lines.push(text(
                        "No agent CLI found on PATH. Install one, then reopen this tour",
                    ));
                    lines.push(text(
                        "from the palette. Until then, worktrees open a shell.",
                    ));
                }
            }
        }
        lines.push(blank());
        lines.push(text(&format!(
            "One key: {} names a branch, you pick the agent, and it starts in a fresh",
            self.hints.new_worktree
        )));
        lines.push(text(&format!(
            "worktree under the {} tier, away from your other checkouts.",
            self.tiers[self.tier_sel]
        )));
        if self.tiers[self.tier_sel].starts_with("sealed") {
            lines.push(Line::segs(vec![
                seg(Tok::Hue(Hue::Amber), format!("{} ", glyph(Glyph::Warn))),
                seg(
                    Tok::Slot(S::Text),
                    "sealed has no network: API-backed agents cannot reach their service."
                        .to_string(),
                ),
            ]));
        }
        if let Some(note) = &self.agent_note {
            lines.extend(marked(false, note, width));
        }
        lines.push(faint(
            "Your own [[agents]] entries stay as they are; only the first one changes.",
        ));
        lines
    }

    fn remote_lines(&self) -> Vec<Line> {
        let bullet = |name: &str, what: &str| {
            Line::segs(vec![
                sp(2),
                seg(
                    Tok::Slot(S::Accent),
                    format!("{} ", glyph(Glyph::DotFilled)),
                ),
                seg(Tok::Slot(S::Text), format!("{name:<8}")).bold(),
                seg(Tok::Slot(S::Dim), what.to_string()),
            ])
        };
        vec![
            text("Long agent runs and heavy builds can live on another machine:"),
            blank(),
            bullet(
                "Sprites",
                "persistent microVMs (paid)       provider = \"sprites\"",
            ),
            bullet(
                "Vercel",
                "Vercel Sandbox microVMs          provider = \"vercel\"",
            ),
            bullet("ssh", "any box you can ssh into         [host.<name>]"),
            blank(),
            text("The repo travels as a git bundle, so your GitHub token never enters"),
            text("the VM; `thegn env pull` brings the commits back."),
            blank(),
            faint("Nothing to do now. When you want it:"),
            text("  1. keep the API token in your keyring or an env var, never in config"),
            text("  2. add an [env.<name>] block (help: Environments, Vercel sandboxes)"),
            text(&format!(
                "  3. pick that environment in the new-worktree wizard ({})",
                self.hints.new_worktree
            )),
            blank(),
            faint("This tour never asks for tokens."),
        ]
    }

    fn keys_lines(&self, width: usize) -> Vec<Line> {
        let mut lines = vec![
            text("Press the real chords. The diagram reacts; nothing is created."),
            blank(),
        ];
        for (i, d) in self.drills.iter().enumerate() {
            let current = i == self.drill_at;
            let (mark, mark_fg) = if d.done {
                (format!("{} ", glyph(Glyph::Check)), Tok::Hue(Hue::Green))
            } else if current {
                (format!("{} ", glyph(Glyph::Chevron)), Tok::Slot(S::Accent))
            } else {
                ("  ".to_string(), Tok::Slot(S::Faint))
            };
            let mut segs = vec![
                seg(mark_fg, mark),
                seg(Tok::Slot(S::Accent), format!("{:<14}", d.chord)).bold(),
                seg(
                    Tok::Slot(if current { S::Text } else { S::Dim }),
                    d.label.to_string(),
                ),
            ];
            if d.key.is_none() {
                segs.push(seg(
                    Tok::Slot(S::Faint),
                    "  (not a single key; shown only)".to_string(),
                ));
            } else if current {
                segs.push(seg(Tok::Slot(S::Faint), "  press it now".to_string()));
            }
            lines.push(Line::segs(segs));
        }
        lines.push(blank());
        lines.extend(mini_lines(&self.mini, None, width));
        lines.push(blank());
        match &self.feedback {
            Some((ok, msg)) => lines.extend(marked(*ok, msg, width)),
            None => lines.push(blank()),
        }
        if self.drill_at >= self.drills.len() {
            lines.push(text("That's the daily set. Enter to finish up."));
        }
        lines.push(faint(&format!(
            "{} is help for what has focus; the status bar lists the keys that work now.",
            self.hints.help
        )));
        lines
    }

    fn finish_lines(&self) -> Vec<Line> {
        let changes = self.changes();
        let mut lines = vec![
            text("This is everything the tour will save:"),
            faint(&format!("  {}", self.config_path)),
            blank(),
        ];
        if changes.is_empty() {
            lines.push(text(
                "  nothing: you kept every default. A perfectly good choice.",
            ));
        }
        for c in &changes {
            lines.push(Line::segs(vec![
                sp(2),
                seg(Tok::Slot(S::Text), format!("{:<17}", c.key)),
                seg(Tok::Hue(Hue::Red), format!("- {}", c.old)),
                seg(Tok::Slot(S::Faint), "  ".to_string()),
                seg(Tok::Hue(Hue::Green), format!("+ {}", c.new)).bold(),
            ]));
        }
        if changes
            .iter()
            .any(|c| matches!(c.op, WriteOp::KeymapPreset(_)))
        {
            lines.push(faint(
                "  (the keymap preset lives in thegn's state, not the file)",
            ));
        }
        lines.push(blank());
        lines.push(text("Try this next:"));
        let step = |n: &str, chord: &str, what: &str| {
            Line::segs(vec![
                seg(Tok::Slot(S::Faint), format!("  {n}. ")),
                seg(Tok::Slot(S::Accent), format!("{chord:<14}")).bold(),
                seg(Tok::Slot(S::Text), what.to_string()),
            ])
        };
        lines.push(step(
            "1",
            self.hints.new_worktree.as_str(),
            "start an agent in a fresh worktree",
        ));
        lines.push(step(
            "2",
            self.hints.palette.as_str(),
            "find any action by typing (try \"theme\")",
        ));
        lines.push(step(
            "3",
            "thegn doctor",
            "see what this machine supports, in a shell pane",
        ));
        lines.push(blank());
        lines.push(faint(
            "Undo later with `thegn config edit`, or reopen this tour from the palette.",
        ));
        lines
    }
}

const HUE_NAMES: [&str; 8] = [
    "teal", "magenta", "purple", "green", "amber", "red", "blue", "orange",
];

/// (outside the worktree, home + credentials, network) for a tier.
fn tier_row(tier: &str) -> (&'static str, &'static str, &'static str) {
    match tier {
        "open" => ("read-write", "read-write", "open"),
        "standard" => ("read-only", "readable", "open"),
        "sealed" => ("read-only", "hidden (empty home)", "none"),
        "sealed-tunnel" => ("read-only", "hidden (empty home)", "none; no VPN"),
        _ => ("?", "?", "?"),
    }
}

fn tier_note(tier: &str) -> &'static str {
    match tier {
        "open" => "No sandbox at all: a pane has your whole user session.",
        "standard" => "The default. ~/.ssh, cloud tokens and agent logins stay readable.",
        "sealed" => "Hardened: no network, so API agents and git fetch/push don't work.",
        "sealed-tunnel" => "Like sealed, and refuses to launch while a VPN is configured.",
        _ => "",
    }
}

/// Move `cur` by `delta` in `0..n`, wrapping or clamping.
fn step(cur: usize, delta: i32, n: usize, wrap: bool) -> usize {
    if n == 0 {
        return 0;
    }
    let next = cur as i64 + delta as i64;
    if wrap {
        next.rem_euclid(n as i64) as usize
    } else {
        next.clamp(0, n as i64 - 1) as usize
    }
}

fn segs_width(segs: &[Seg]) -> usize {
    segs.iter()
        .map(|s| UnicodeWidthStr::width(s.text.as_str()))
        .sum()
}

/// Pad (or cut) `s` to exactly `w` display columns.
fn fit(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push_str(&" ".repeat(w - used));
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let tail: String = s
        .chars()
        .rev()
        .take(max - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{}{tail}", glyph(Glyph::Ellipsis))
}

/// Greedy word wrap to `w` columns (a word longer than `w` gets its own line).
fn wrap(t: &str, w: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in t.split_whitespace() {
        let need = UnicodeWidthStr::width(cur.as_str()) + 1 + UnicodeWidthStr::width(word);
        if !cur.is_empty() && need > w {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A status message: a marker glyph, then `msg` wrapped under it.
fn marked(ok: bool, msg: &str, width: usize) -> Vec<Line> {
    let mark = if ok {
        seg(Tok::Hue(Hue::Green), format!("{} ", glyph(Glyph::Check)))
    } else {
        seg(Tok::Hue(Hue::Amber), format!("{} ", glyph(Glyph::Warn)))
    };
    wrap(msg, width.saturating_sub(2).max(20))
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            Line::segs(vec![
                if i == 0 { mark.clone() } else { sp(2) },
                seg(Tok::Slot(S::Text), l),
            ])
        })
        .collect()
}

fn text(t: &str) -> Line {
    Line::segs(vec![seg(Tok::Slot(S::Text), t.to_string())])
}

fn faint(t: &str) -> Line {
    Line::segs(vec![seg(Tok::Slot(S::Faint), t.to_string())])
}

fn blank() -> Line {
    Line::segs(vec![sp(0)])
}

/// The practice workspace drawn as a sidebar + tab strip + pane boxes, with
/// `hl` highlighted. Box glyphs come from the active glyph set (ASCII-safe).
fn mini_lines(m: &Mini, hl: Option<Part>, width: usize) -> Vec<Line> {
    const LEFT: usize = 20;
    let right = width.saturating_sub(LEFT + 2).clamp(12, 46);
    let on = |p: Part| hl == Some(p);
    let tone = |p: Part| {
        if on(p) {
            Tok::Slot(S::Accent)
        } else {
            Tok::Slot(S::Dim)
        }
    };
    let panes = m.worktrees.get(m.active).map(|w| w.1).unwrap_or(1).max(1);
    let gap = 1;
    let box_w = (right.saturating_sub(gap * (panes - 1)) / panes).max(4);
    let inner = box_w - 2;
    let (h, v) = (glyph(Glyph::BoxH), glyph(Glyph::BoxV));
    let pane_row = |l: &str, mid: &str, r: &str| -> Vec<Seg> {
        let mut segs = Vec::new();
        for i in 0..panes {
            if i > 0 {
                segs.push(sp(gap));
            }
            segs.push(seg(
                tone(Part::Pane),
                format!("{l}{}{r}", mid.repeat(inner)),
            ));
        }
        segs
    };
    let label_row = |first: bool| -> Vec<Seg> {
        let mut segs = Vec::new();
        let wt = m
            .worktrees
            .get(m.active)
            .map(|w| w.0.as_str())
            .unwrap_or("main");
        for i in 0..panes {
            if i > 0 {
                segs.push(sp(gap));
            }
            let label = if !first {
                String::new()
            } else if wt != "main" && i == 0 {
                "agent".to_string()
            } else {
                "$ _".to_string()
            };
            segs.push(seg(tone(Part::Pane), v.to_string()));
            segs.push(seg(Tok::Slot(S::Text), fit(&format!(" {label}"), inner)));
            segs.push(seg(tone(Part::Pane), v.to_string()));
        }
        segs
    };
    // Left column: project header, then one row per worktree.
    let mut left: Vec<Vec<Seg>> = vec![
        vec![seg(Tok::Slot(S::Faint), fit("SIDEBAR", LEFT))],
        vec![seg(
            tone(Part::Project),
            fit(&format!("{} thegn", glyph(Glyph::CaretOpen)), LEFT),
        )],
    ];
    for (i, (name, _)) in m.worktrees.iter().enumerate() {
        let dot = if i == m.active {
            glyph(Glyph::DotFilled)
        } else {
            glyph(Glyph::DotHollow)
        };
        left.push(vec![seg(
            tone(Part::Worktree),
            fit(&format!("  {dot} {name}"), LEFT),
        )]);
    }
    // Right column: tab strip, then the active worktree's panes.
    let mut tabs = Vec::new();
    for (i, (name, _)) in m.worktrees.iter().enumerate() {
        let active = i == m.active;
        let s = if active {
            format!("[{name}]")
        } else {
            format!(" {name} ")
        };
        let fg = if active && (on(Part::Tab) || on(Part::Worktree)) {
            Tok::Slot(S::Accent)
        } else if active {
            Tok::Slot(S::Text)
        } else {
            Tok::Slot(S::Faint)
        };
        let mut t = seg(fg, s);
        if active {
            t = t.bold();
        }
        tabs.push(t);
        tabs.push(sp(1));
    }
    let mut rightc: Vec<Vec<Seg>> = vec![
        tabs,
        pane_row(glyph(Glyph::BoxTl), h, glyph(Glyph::BoxTr)),
        label_row(true),
        label_row(false),
        pane_row(glyph(Glyph::BoxBl), h, glyph(Glyph::BoxBr)),
    ];
    if m.palette {
        rightc.push(vec![
            seg(Tok::Slot(S::Accent), format!("{} ", glyph(Glyph::Chevron))).bold(),
            seg(
                Tok::Slot(S::Text),
                "palette: type to search every action".to_string(),
            ),
        ]);
    }
    let rows = left.len().max(rightc.len());
    (0..rows)
        .map(|r| {
            let mut segs = vec![sp(2)];
            match left.get(r) {
                Some(l) => segs.extend(l.iter().cloned()),
                None => segs.push(sp(LEFT)),
            }
            if let Some(rc) = rightc.get(r) {
                segs.extend(rc.iter().cloned());
            }
            Line::segs(segs)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    /// A config as loaded at runtime: the built-in `claude` + `shell` agents.
    fn cfg() -> Config {
        toml::from_str(
            "[[agents]]\nname = \"claude\"\ncommand = \"claude\"\n\
             [[agents]]\nname = \"shell\"\ncommand = \"__shell__\"\n",
        )
        .unwrap()
    }

    fn tour() -> WelcomeTour {
        WelcomeTour::new(&cfg())
    }

    fn press(t: &mut WelcomeTour, k: KeyCode) -> TourOutcome {
        t.handle_key(&k, NONE)
    }

    fn agents(installed: &[&str]) -> ProbeResult {
        ProbeResult::Agents(
            ["claude", "codex", "pi"]
                .iter()
                .map(|id| AgentInfo {
                    id: id.to_string(),
                    name: id.to_string(),
                    command: id.to_string(),
                    path: installed.contains(id).then(|| format!("/usr/bin/{id}")),
                })
                .collect(),
        )
    }

    fn goto(t: &mut WelcomeTour, target: Chapter) {
        for _ in 0..CHAPTERS.len() {
            if t.chapter() == target {
                return;
            }
            press(t, KeyCode::Enter);
        }
        assert_eq!(t.chapter(), target);
    }

    fn writes(o: TourOutcome) -> Vec<WriteOp> {
        match o {
            TourOutcome::Close { writes, .. } => writes,
            o => panic!("expected close, got {o:?}"),
        }
    }

    #[test]
    fn esc_on_welcome_skips_everything_without_writes() {
        let mut t = tour();
        assert_eq!(
            press(&mut t, KeyCode::Escape),
            TourOutcome::Close {
                completed: false,
                writes: vec![],
                open_setup: false
            }
        );
    }

    #[test]
    fn esc_mid_tour_discards_choices() {
        let mut t = tour();
        goto(&mut t, Chapter::Look);
        press(&mut t, KeyCode::DownArrow);
        assert!(!t.changes().is_empty());
        assert!(writes(press(&mut t, KeyCode::Escape)).is_empty());
    }

    #[test]
    fn esc_leaves_from_every_chapter() {
        // The skip control on every chapter names `esc`; the key handler must
        // honour it there too (the drill included).
        for c in CHAPTERS {
            let mut t = tour();
            t.chapter = *c;
            assert_eq!(
                press(&mut t, KeyCode::Escape),
                TourOutcome::Close {
                    completed: false,
                    writes: vec![],
                    open_setup: false
                },
                "{c:?} must close on the skip chord"
            );
        }
    }

    #[test]
    fn every_chapter_paints_the_skip_control() {
        let mut t = tour();
        for c in CHAPTERS {
            t.chapter = *c;
            let mut s = Surface::new(80, 24);
            t.render(
                &mut s,
                Rect {
                    x: 0,
                    y: 0,
                    cols: 80,
                    rows: 24,
                },
            );
            let screen = s.screen_chars_to_string();
            assert!(
                screen.contains("skip tour"),
                "{c:?} has no visible skip control:\n{screen}"
            );
        }
    }

    #[test]
    fn walking_through_unchanged_writes_nothing() {
        let mut t = tour();
        goto(&mut t, Chapter::Finish);
        let o = press(&mut t, KeyCode::Enter);
        assert!(matches!(
            o,
            TourOutcome::Close {
                completed: true,
                ..
            }
        ));
        assert!(writes(o).is_empty());
    }

    #[test]
    fn leaving_welcome_starts_both_probes_once() {
        let mut t = tour();
        match press(&mut t, KeyCode::Enter) {
            TourOutcome::Do(fx) => {
                assert!(fx.probes.contains(&ProbeRequest::Agents));
                assert!(
                    fx.probes
                        .iter()
                        .any(|p| matches!(p, ProbeRequest::Sandbox(chain) if !chain.contains(&"auto".to_string())))
                );
            }
            o => panic!("expected probes, got {o:?}"),
        }
        match press(&mut t, KeyCode::Enter) {
            TourOutcome::Do(fx) => assert!(fx.probes.is_empty()),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn theme_moves_preview_live_and_skip_reverts_it() {
        let mut t = tour();
        goto(&mut t, Chapter::Look);
        match press(&mut t, KeyCode::DownArrow) {
            TourOutcome::Do(fx) => assert_eq!(fx.preview_theme.as_deref(), Some("storm")),
            o => panic!("{o:?}"),
        }
        assert_eq!(t.changes()[0].key, "theme.preset");
        match press(&mut t, KeyCode::Tab) {
            TourOutcome::Do(fx) => assert_eq!(fx.preview_theme.as_deref(), Some("prism")),
            o => panic!("{o:?}"),
        }
        assert_eq!(t.chapter(), Chapter::Sandbox);
        assert!(t.changes().is_empty());
    }

    #[test]
    fn finish_writes_theme_keymap_and_tier() {
        let mut t = tour();
        goto(&mut t, Chapter::Look);
        press(&mut t, KeyCode::DownArrow); // storm
        press(&mut t, KeyCode::RightArrow); // vscode
        press(&mut t, KeyCode::Enter);
        assert_eq!(t.chapter(), Chapter::Sandbox);
        press(&mut t, KeyCode::DownArrow); // standard → sealed
        goto(&mut t, Chapter::Finish);
        let w = writes(press(&mut t, KeyCode::Enter));
        assert_eq!(
            w,
            vec![
                WriteOp::Set {
                    key: "theme.preset".into(),
                    value: "storm".into()
                },
                WriteOp::KeymapPreset("vscode".into()),
                WriteOp::Set {
                    key: "sandbox.profile".into(),
                    value: "sealed".into()
                },
            ]
        );
    }

    #[test]
    fn backend_cycles_and_r_rechecks() {
        let mut t = tour();
        goto(&mut t, Chapter::Sandbox);
        press(&mut t, KeyCode::RightArrow);
        assert!(t.changes().iter().any(|c| c.key == "sandbox.backend"));
        t.apply_probe(ProbeResult::Sandbox(vec![]));
        assert!(matches!(
            press(&mut t, KeyCode::Char('r')),
            TourOutcome::Do(TourEffects { ref probes, .. }) if probes.len() == 1
        ));
        // A second `r` while the re-probe is in flight does nothing.
        assert_eq!(press(&mut t, KeyCode::Char('r')), TourOutcome::Pending);
    }

    #[test]
    fn agent_pick_promotes_an_installed_harness() {
        let mut t = tour();
        goto(&mut t, Chapter::Agents);
        t.apply_probe(agents(&["claude", "codex"]));
        // The built-in default is claude, so it is preselected: no change.
        assert!(t.changes().is_empty());
        press(&mut t, KeyCode::DownArrow); // codex
        press(&mut t, KeyCode::Enter);
        assert_eq!(t.chapter(), Chapter::Remote);
        let c = t.changes();
        assert_eq!(c.len(), 1);
        match &c[0].op {
            WriteOp::PromoteAgent {
                name,
                command,
                keep,
            } => {
                assert_eq!(name, "codex");
                assert_eq!(command, "codex");
                assert!(keep.iter().any(|(n, _)| n == "shell"));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn agent_pick_refuses_a_missing_harness() {
        let mut t = tour();
        goto(&mut t, Chapter::Agents);
        t.apply_probe(agents(&["claude"]));
        press(&mut t, KeyCode::DownArrow); // codex, not installed
        assert_eq!(press(&mut t, KeyCode::Enter), TourOutcome::Pending);
        assert_eq!(t.chapter(), Chapter::Agents);
        assert!(t.agent_note.is_some());
        assert!(t.changes().is_empty());
    }

    #[test]
    fn drill_accepts_the_live_chord_and_mutates_the_practice_workspace() {
        let mut t = tour();
        goto(&mut t, Chapter::Keys);
        let first = t.drills[0].key.clone().expect("new-worktree has a chord");
        assert_eq!(t.mini.worktrees.len(), 1);
        t.handle_key(&first.code, first.mods);
        assert!(t.drills[0].done);
        assert_eq!(t.mini.worktrees.len(), 2);
        assert_eq!(t.drill_at, 1);
        assert_eq!(t.chapter(), Chapter::Keys);
    }

    #[test]
    fn drill_names_the_miss_and_then_the_likely_cause() {
        let mut t = tour();
        goto(&mut t, Chapter::Keys);
        t.handle_key(&KeyCode::Char('q'), Modifiers::ALT);
        let (ok, msg) = t.feedback.clone().unwrap();
        assert!(!ok);
        assert!(msg.contains("Alt-q"), "{msg}");
        t.handle_key(&KeyCode::Char('q'), Modifiers::ALT);
        t.handle_key(&KeyCode::Char('q'), Modifiers::ALT);
        let (_, msg) = t.feedback.clone().unwrap();
        assert!(msg.contains("terminal-compatibility"), "{msg}");
        assert_eq!(t.drill_at, 0);
    }

    #[test]
    fn drill_follows_a_rebound_chord() {
        let mut cfg = cfg();
        cfg.keybinds
            .normal
            .insert("new-worktree".to_string(), "Alt n".to_string());
        let t = WelcomeTour::new(&cfg);
        assert_eq!(t.drills[0].chord, "Alt-n");
        assert_eq!(
            t.drills[0].key,
            Some(Key::modified(KeyCode::Char('n'), Modifiers::ALT))
        );
    }

    #[test]
    fn a_and_back_navigation() {
        let mut t = tour();
        assert!(matches!(
            press(&mut t, KeyCode::Char('a')),
            TourOutcome::Close {
                open_setup: true,
                completed: false,
                ..
            }
        ));
        let mut t = tour();
        goto(&mut t, Chapter::Sandbox);
        t.handle_key(&KeyCode::Tab, Modifiers::SHIFT);
        assert_eq!(t.chapter(), Chapter::Look);
        press(&mut t, KeyCode::Backspace);
        assert_eq!(t.chapter(), Chapter::Workspace);
        // Back from Welcome is a no-op, never a close.
        let mut t = tour();
        assert_eq!(press(&mut t, KeyCode::Backspace), TourOutcome::Pending);
    }

    #[test]
    fn every_chapter_fits_80x24() {
        let mut t = tour();
        t.apply_probe(agents(&["claude", "codex", "pi"]));
        t.apply_probe(ProbeResult::Sandbox(vec![]));
        for c in CHAPTERS {
            t.chapter = *c;
            // Body budget at 24 rows: header + gap above, gap + footer below.
            let extra = if *c == Chapter::Welcome { 4 } else { 0 };
            let n = t.body_lines(76).len() + extra;
            assert!(n <= 24 - 4, "{c:?} body is {n} rows");
        }
        t.chapter = Chapter::Keys;
        t.mini.palette = true;
        assert!(t.body_lines(76).len() <= 20);
    }

    #[test]
    fn mini_workspace_caps_and_closes() {
        let mut m = Mini::single();
        for _ in 0..5 {
            m.apply(DrillKind::NewWorktree);
            m.apply(DrillKind::Split);
        }
        assert_eq!(m.worktrees.len(), MAX_MINI_WORKTREES);
        assert_eq!(m.worktrees[m.active].1, MAX_MINI_PANES);
        m.apply(DrillKind::Palette);
        m.apply(DrillKind::Close);
        assert!(!m.palette);
        assert_eq!(m.worktrees[m.active].1, MAX_MINI_PANES - 1);
    }

    #[test]
    fn describe_names_chords_like_the_hints() {
        assert_eq!(
            describe(&Key::modified(KeyCode::Char(' '), Modifiers::CTRL)),
            "Ctrl-Space"
        );
        assert_eq!(
            describe(&Key::modified(KeyCode::Char('w'), Modifiers::ALT)),
            "Alt-w"
        );
    }
}
