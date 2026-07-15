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
    ForgeName,
    ForgeKind,
    ForgeHost,
    ForgeToken,
    ForgeAccountAction,
    // Issues
    IssuesProvider,
    IssuesName,
    IssuesToken,
    LinearTeam,
    JiraUrl,
    JiraEmail,
    JiraProject,
    IssuesAction,
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
    /// `config_write::upsert_host`: `[host.<name>]` + `[host.<name>.ssh]`.
    Host { name: String, ssh: String },
    /// The keymap preset (persisted to `ui_state`, not the config file).
    KeymapPreset(String),
    /// `config_write::upsert_issue_account`: a `[[issue_accounts]]` entry.
    /// `token` is a RAW token to be stored via the secret backend unless
    /// `token_is_ref` (then it's an existing SecretRef, written verbatim —
    /// used when materializing a legacy single-provider config into an account).
    UpsertIssueAccount {
        name: String,
        provider: String,
        token: String,
        token_is_ref: bool,
        team_id: String,
        workspace_slug: String,
        base_url: String,
        email: String,
        project_key: String,
    },
    /// `config_write::upsert_forge`: a `[[forges]]` entry. `token` raw unless
    /// `token_is_ref`.
    UpsertForge {
        name: String,
        kind: String,
        host: String,
        token: String,
        token_is_ref: bool,
    },
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
    /// Live-apply this theme preset to the runtime palette (preview only, not
    /// persisted): set while cycling the Appearance step's theme field.
    pub preview_theme: Option<String>,
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
const FORGE_KINDS: &[&str] = &["github", "ghe", "forgejo", "gitea"];

/// Push a blank spacer line followed by `line` (a small body-lines helper).
fn blank_then(lines: &mut Vec<Line>, line: Line) {
    lines.push(Line::segs(vec![sp(0)]));
    lines.push(line);
}
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
    // Paths
    worktrees_dir: String,
    repo_roots: String,
    // Forge
    forge: Probe<ForgeStatus>,
    forge_action: usize,
    // Forge accounts (`[[forges]]`) — draft + running list of (name, kind).
    forge_name: String,
    forge_kind_sel: usize,
    forge_host: String,
    forge_token: String,
    forge_account_action: usize,
    forges: Vec<(String, String)>,
    // Issues
    issues_sel: usize,
    issue_name: String,
    issues_token: String,
    linear_team: String,
    jira_url: String,
    jira_email: String,
    jira_project: String,
    issues_action: usize,
    // Issue accounts (`[[issue_accounts]]`) — running list of (name, provider).
    issue_accounts: Vec<(String, String)>,
    // Writes that materialize a legacy single-provider config into explicit
    // `[[issue_accounts]]` on the first user add, so switching to explicit
    // accounts never silently drops the legacy provider. Emitted once.
    legacy_issue_seed: Vec<WriteOp>,
    materialized_legacy: bool,
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
        // Explicit issue accounts already in config → the display list.
        let issue_accounts: Vec<(String, String)> = cfg
            .issues
            .issue_accounts
            .iter()
            .map(|a| (a.name.clone(), a.provider.as_str().to_string()))
            .collect();
        // A legacy single-provider config (no explicit accounts) synthesizes
        // accounts; capture their writes so the first user add materializes them
        // as explicit entries (tokens are already SecretRefs → written verbatim).
        let legacy_issue_seed: Vec<WriteOp> = if cfg.issues.issue_accounts.is_empty() {
            cfg.issues
                .active_accounts()
                .into_iter()
                .map(|a| WriteOp::UpsertIssueAccount {
                    name: a.name,
                    provider: a.provider.as_str().to_string(),
                    token: a.token,
                    token_is_ref: true,
                    team_id: a.team_id,
                    workspace_slug: a.workspace_slug,
                    base_url: a.base_url,
                    email: a.email,
                    project_key: a.project_key,
                })
                .collect()
        } else {
            Vec::new()
        };
        let forges: Vec<(String, String)> = cfg
            .forges
            .iter()
            .map(|f| (f.name.clone(), f.kind.as_str().to_string()))
            .collect();
        OnboardingWizard {
            step: Step::Welcome,
            focus: Field::ForgeAction, // unused until a fielded step; reset on entry
            init_worktrees_dir: cfg.worktrees_dir.clone(),
            init_repo_roots: repo_roots.clone(),
            init_theme: theme.clone(),
            init_keymap: keymap.clone(),
            init_backend: backend.clone(),
            init_profile: profile.clone(),
            worktrees_dir: cfg.worktrees_dir.clone(),
            repo_roots,
            forge: Probe::Idle,
            forge_action: 0,
            forge_name: String::new(),
            forge_kind_sel: 0,
            forge_host: String::new(),
            forge_token: String::new(),
            forge_account_action: 0,
            forges,
            // The draft provider starts at "none" so a re-run doesn't re-add an
            // existing account; the display list shows what's already configured.
            issues_sel: pos(ISSUE_PROVIDERS, "none"),
            issue_name: String::new(),
            issues_token: String::new(),
            linear_team: String::new(),
            jira_url: String::new(),
            jira_email: String::new(),
            jira_project: String::new(),
            issues_action: 0,
            issue_accounts,
            legacy_issue_seed,
            materialized_legacy: false,
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
            Step::Forge => {
                let mut f = vec![Field::ForgeAction, Field::ForgeName, Field::ForgeKind];
                // github.com needs no host/token (the `gh` CLI owns auth); the
                // others take a host + optional token.
                if self.forge_kind() != "github" {
                    f.extend([Field::ForgeHost, Field::ForgeToken]);
                }
                f.push(Field::ForgeAccountAction);
                f
            }
            Step::Issues => {
                let mut f = vec![Field::IssuesProvider];
                match self.issues_provider() {
                    "linear" => {
                        f.extend([Field::IssuesName, Field::IssuesToken, Field::LinearTeam])
                    }
                    "jira" => f.extend([
                        Field::IssuesName,
                        Field::IssuesToken,
                        Field::JiraUrl,
                        Field::JiraEmail,
                        Field::JiraProject,
                    ]),
                    "github" => f.push(Field::IssuesName),
                    _ => {}
                }
                // The action row (add account / continue) is always the last row
                // once a provider is chosen; with "none" the provider row is the
                // only field and Enter advances.
                if self.issues_provider() != "none" {
                    f.push(Field::IssuesAction);
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
            // Issues + Forge accounts are committed as `[[issue_accounts]]` /
            // `[[forges]]` entries by `commit_issue_draft`/`commit_forge_draft`
            // (via the action row or `advance`), not here.
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
        let mut writes = self.leave_writes();
        // Commit any filled-but-not-explicitly-added issue/forge draft so a user
        // who fills the form and just hits "continue" doesn't lose it.
        if self.step == Step::Issues {
            writes.extend(self.commit_issue_draft());
        }
        if self.step == Step::Forge {
            writes.extend(self.commit_forge_draft());
        }
        let mut effects = Effects {
            writes,
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

    fn forge_kind(&self) -> &'static str {
        FORGE_KINDS[self.forge_kind_sel.min(FORGE_KINDS.len() - 1)]
    }

    /// The issue action-row options: "add account" appears only when the draft
    /// holds enough to save.
    fn issues_actions(&self) -> Vec<&'static str> {
        if self.issue_draft_has_content() {
            vec!["add account", "continue"]
        } else {
            vec!["continue"]
        }
    }

    /// The forge action-row options (the `[[forges]]` list, separate from the
    /// `gh` auth action at the top of the step).
    fn forge_account_actions(&self) -> Vec<&'static str> {
        if self.forge_draft_has_content() {
            vec!["add forge", "continue"]
        } else {
            vec!["continue"]
        }
    }

    /// Does the issue draft hold enough to be worth saving as an account?
    /// GitHub needs nothing but the provider choice; the rest need a token.
    fn issue_draft_has_content(&self) -> bool {
        match self.issues_provider() {
            "none" => false,
            "github" => true,
            _ => !self.issues_token.trim().is_empty(),
        }
    }

    /// A forge draft is saveable once it has a name.
    fn forge_draft_has_content(&self) -> bool {
        !self.forge_name.trim().is_empty()
    }

    /// Commit the current issue draft as a `[[issue_accounts]]` write (plus, on
    /// the first add, the legacy-provider materialization), then clear the draft
    /// (provider → none) so it isn't re-added. Returns the writes to apply.
    fn commit_issue_draft(&mut self) -> Vec<WriteOp> {
        if !self.issue_draft_has_content() {
            return Vec::new();
        }
        let provider = self.issues_provider().to_string();
        let name = if self.issue_name.trim().is_empty() {
            let n = self
                .issue_accounts
                .iter()
                .filter(|(_, p)| *p == provider)
                .count()
                + 1;
            format!("{provider}-{n}")
        } else {
            self.issue_name.trim().to_string()
        };
        let mut writes = Vec::new();
        // First explicit add while a legacy provider exists: materialize the
        // legacy accounts so switching to explicit mode doesn't drop them.
        if !self.materialized_legacy {
            writes.append(&mut self.legacy_issue_seed);
            self.materialized_legacy = true;
        }
        writes.push(WriteOp::UpsertIssueAccount {
            name: name.clone(),
            provider: provider.clone(),
            token: self.issues_token.trim().to_string(),
            token_is_ref: false,
            team_id: self.linear_team.trim().to_string(),
            workspace_slug: String::new(),
            base_url: self.jira_url.trim().to_string(),
            email: self.jira_email.trim().to_string(),
            project_key: self.jira_project.trim().to_string(),
        });
        if !self.issue_accounts.iter().any(|(n, _)| *n == name) {
            self.issue_accounts.push((name, provider));
        }
        self.issue_name.clear();
        self.issues_token.clear();
        self.linear_team.clear();
        self.jira_url.clear();
        self.jira_email.clear();
        self.jira_project.clear();
        self.issues_sel = 0; // reset to "none" so it isn't re-added on advance
        self.issues_action = 0;
        // Focus back to the provider row — the action row is gone now that the
        // draft (provider) reset to "none".
        self.focus = Field::IssuesProvider;
        writes
    }

    /// Commit the current forge draft as a `[[forges]]` write, then clear it.
    fn commit_forge_draft(&mut self) -> Vec<WriteOp> {
        if !self.forge_draft_has_content() {
            return Vec::new();
        }
        let name = self.forge_name.trim().to_string();
        let kind = self.forge_kind().to_string();
        let op = WriteOp::UpsertForge {
            name: name.clone(),
            kind: kind.clone(),
            host: self.forge_host.trim().to_string(),
            token: self.forge_token.trim().to_string(),
            token_is_ref: false,
        };
        if !self.forges.iter().any(|(n, _)| *n == name) {
            self.forges.push((name, kind));
        }
        self.forge_name.clear();
        self.forge_host.clear();
        self.forge_token.clear();
        self.forge_account_action = 0;
        self.focus = Field::ForgeName; // back to the top of the draft
        vec![op]
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
            Field::IssuesName => Some(&mut self.issue_name),
            Field::ForgeName => Some(&mut self.forge_name),
            Field::ForgeHost => Some(&mut self.forge_host),
            Field::ForgeToken => Some(&mut self.forge_token),
            Field::HostName => Some(&mut self.host_name),
            Field::HostSsh => Some(&mut self.host_ssh),
            _ => None,
        }
    }

    fn is_cycle(f: Field) -> bool {
        matches!(
            f,
            Field::ForgeAction
                | Field::ForgeKind
                | Field::ForgeAccountAction
                | Field::IssuesProvider
                | Field::IssuesAction
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
            Field::ForgeKind => self.forge_kind_sel = wrap(self.forge_kind_sel, FORGE_KINDS.len()),
            Field::ForgeAccountAction => {
                self.forge_account_action = wrap(
                    self.forge_account_action,
                    self.forge_account_actions().len(),
                )
            }
            Field::IssuesProvider => self.issues_sel = wrap(self.issues_sel, ISSUE_PROVIDERS.len()),
            Field::IssuesAction => {
                self.issues_action = wrap(self.issues_action, self.issues_actions().len())
            }
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
            KeyCode::LeftArrow if Self::is_cycle(self.focus) => {
                self.cycle(-1);
                if let Some(o) = self.theme_preview_outcome() {
                    return o;
                }
            }
            KeyCode::RightArrow if Self::is_cycle(self.focus) => {
                self.cycle(1);
                if let Some(o) = self.theme_preview_outcome() {
                    return o;
                }
            }
            KeyCode::Enter => {
                // Forge gh-auth action row (login / re-check / continue).
                if self.focus == Field::ForgeAction {
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
                // Issue / forge account action rows: add-and-stay, or continue.
                if self.focus == Field::IssuesAction {
                    return match self.issues_actions().get(self.issues_action).copied() {
                        Some("add account") => Outcome::Do(Effects {
                            writes: self.commit_issue_draft(),
                            ..Effects::default()
                        }),
                        _ => self.advance(),
                    };
                }
                if self.focus == Field::ForgeAccountAction {
                    return match self
                        .forge_account_actions()
                        .get(self.forge_account_action)
                        .copied()
                    {
                        Some("add forge") => Outcome::Do(Effects {
                            writes: self.commit_forge_draft(),
                            ..Effects::default()
                        }),
                        _ => self.advance(),
                    };
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

    /// When the Appearance theme field is focused, an effect that live-applies
    /// the currently selected preset to the runtime palette. Preview only —
    /// persistence still rides `leave_writes` on advance past the step.
    fn theme_preview_outcome(&self) -> Option<Outcome> {
        (self.focus == Field::ThemePreset).then(|| {
            Outcome::Do(Effects {
                preview_theme: Some(thegn_core::theme::PRESETS[self.theme_sel].to_string()),
                ..Effects::default()
            })
        })
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
                let mut lines = vec![
                    Line::segs(status),
                    blank(),
                    Line::segs(self.cycle_row(
                        "gh auth   ",
                        self.focus == Field::ForgeAction,
                        action,
                    )),
                ];
                lines.extend(self.configured_list("forges", &self.forges));
                blank_then(
                    &mut lines,
                    note("add a forge (github.com, enterprise, forgejo/gitea):"),
                );
                lines.push(self.text_row(
                    Field::ForgeName,
                    "name      ",
                    &self.forge_name,
                    "· skip — no extra forge",
                    false,
                ));
                lines.push(Line::segs(self.cycle_row(
                    "kind      ",
                    self.focus == Field::ForgeKind,
                    self.forge_kind(),
                )));
                if self.forge_kind() != "github" {
                    lines.push(self.text_row(
                        Field::ForgeHost,
                        "host      ",
                        &self.forge_host,
                        "git.example.com",
                        false,
                    ));
                    lines.push(self.text_row(
                        Field::ForgeToken,
                        "token     ",
                        &self.forge_token,
                        "· optional (stored securely)",
                        true,
                    ));
                }
                let facction = self
                    .forge_account_actions()
                    .get(self.forge_account_action)
                    .copied()
                    .unwrap_or("continue");
                lines.push(Line::segs(self.cycle_row(
                    "          ",
                    self.focus == Field::ForgeAccountAction,
                    facction,
                )));
                blank_then(
                    &mut lines,
                    note("non-github forges are config-only for now (fetch: github)."),
                );
                lines
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
                            Field::IssuesName,
                            "name      ",
                            &self.issue_name,
                            "· auto (linear-N)",
                            false,
                        ));
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
                            Field::IssuesName,
                            "name      ",
                            &self.issue_name,
                            "· auto (jira-N)",
                            false,
                        ));
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
                    "github" => {
                        lines.push(self.text_row(
                            Field::IssuesName,
                            "name      ",
                            &self.issue_name,
                            "· auto (github-N)",
                            false,
                        ));
                        lines.push(note("github issues auto-scope to each repo's remote."));
                    }
                    _ => {}
                }
                if self.issues_provider() != "none" {
                    let action = self
                        .issues_actions()
                        .get(self.issues_action)
                        .copied()
                        .unwrap_or("continue");
                    lines.push(Line::segs(self.cycle_row(
                        "          ",
                        self.focus == Field::IssuesAction,
                        action,
                    )));
                }
                lines.extend(self.configured_list("accounts", &self.issue_accounts));
                let store = if self.keyring {
                    "OS keyring"
                } else {
                    "0600 file"
                };
                blank_then(
                    &mut lines,
                    note(&format!(
                        "add one or more accounts (tokens → {store}); all aggregate."
                    )),
                );
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

    /// Render the running list of already-configured accounts/forges as a
    /// header + one bullet per entry. Empty ⇒ no lines.
    fn configured_list(&self, label: &str, items: &[(String, String)]) -> Vec<Line> {
        if items.is_empty() {
            return Vec::new();
        }
        let mut out = vec![Line::segs(vec![sp(0)])];
        out.push(Line::segs(vec![seg(
            Tok::Slot(S::Faint),
            format!("{label} ({}):", items.len()),
        )]));
        for (name, kind) in items {
            out.push(Line::segs(vec![
                sp(2),
                seg(Tok::Hue(thegn_core::theme::Hue::Green), "● ".to_string()),
                seg(Tok::Slot(S::Text), name.clone()),
                seg(Tok::Slot(S::Faint), format!("  · {kind}")),
            ]));
        }
        out
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
    fn issues_provider_gates_fields_and_adds_account() {
        let mut w = wiz();
        goto(&mut w, Step::Issues);
        assert_eq!(w.fields(), vec![Field::IssuesProvider]);
        // cycle none → github → linear
        w.handle_key(&KeyCode::RightArrow, NONE);
        w.handle_key(&KeyCode::RightArrow, NONE);
        assert_eq!(w.issues_provider(), "linear");
        assert_eq!(
            w.fields(),
            vec![
                Field::IssuesProvider,
                Field::IssuesName,
                Field::IssuesToken,
                Field::LinearTeam,
                Field::IssuesAction,
            ]
        );
        enter(&mut w); // → name
        enter(&mut w); // → token
        typ(&mut w, "lin_api_SECRET");
        enter(&mut w); // → team
        enter(&mut w); // → action row
        assert_eq!(w.focus, Field::IssuesAction);
        // The draft has a token, so "add account" is offered and selected.
        assert_eq!(w.issues_actions(), vec!["add account", "continue"]);
        match enter(&mut w) {
            Outcome::Do(e) => {
                assert!(e.writes.iter().any(|op| matches!(
                    op,
                    WriteOp::UpsertIssueAccount { provider, token, token_is_ref, name, .. }
                        if provider == "linear" && token == "lin_api_SECRET"
                            && !token_is_ref && name == "linear-1"
                )));
            }
            o => panic!("expected Do, got {o:?}"),
        }
        // After adding, the draft resets to "none" and the account is listed —
        // so a second account can be added without clobbering the first.
        assert_eq!(w.issues_provider(), "none");
        assert_eq!(w.issue_accounts.len(), 1);
    }

    #[test]
    fn issues_can_add_two_accounts_of_one_provider() {
        let mut w = wiz();
        goto(&mut w, Step::Issues);
        // Add first Linear account.
        for _ in 0..2 {
            w.handle_key(&KeyCode::RightArrow, NONE); // → linear
        }
        enter(&mut w); // name
        enter(&mut w); // token
        typ(&mut w, "tok1");
        enter(&mut w); // team
        enter(&mut w); // action
        assert!(matches!(enter(&mut w), Outcome::Do(_))); // add
        // Add a second Linear account.
        for _ in 0..2 {
            w.handle_key(&KeyCode::RightArrow, NONE); // none → github → linear
        }
        enter(&mut w); // name
        enter(&mut w); // token
        typ(&mut w, "tok2");
        enter(&mut w); // team
        enter(&mut w); // action
        assert!(matches!(enter(&mut w), Outcome::Do(_))); // add
        assert_eq!(w.issue_accounts.len(), 2);
        assert_eq!(w.issue_accounts[0].0, "linear-1");
        assert_eq!(w.issue_accounts[1].0, "linear-2");
    }

    #[test]
    fn forge_step_adds_a_named_forge() {
        let mut w = wiz();
        goto(&mut w, Step::Forge);
        // Focus starts on the gh-auth action; walk to the forge name field.
        w.move_focus(1);
        assert_eq!(w.focus, Field::ForgeName);
        typ(&mut w, "corp");
        w.move_focus(1); // → kind
        w.handle_key(&KeyCode::RightArrow, NONE); // github → ghe
        assert_eq!(w.forge_kind(), "ghe");
        assert_eq!(w.fields().last(), Some(&Field::ForgeAccountAction));
        // Navigate to the account action row and add.
        while w.focus != Field::ForgeAccountAction {
            w.move_focus(1);
        }
        assert_eq!(w.forge_account_actions(), vec!["add forge", "continue"]);
        match enter(&mut w) {
            Outcome::Do(e) => assert!(e.writes.iter().any(|op| matches!(
                op,
                WriteOp::UpsertForge { name, kind, .. } if name == "corp" && kind == "ghe"
            ))),
            o => panic!("expected Do, got {o:?}"),
        }
        assert_eq!(w.forges.len(), 1);
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
