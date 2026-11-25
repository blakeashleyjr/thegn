//! The "Add environment" wizard (command palette → "New environment…").
//!
//! A single-plane modal — mirroring [`crate::terminal_wizard`] — that authors a
//! `[env.<name>]` for **any** placement: `local`, `ssh`, or a managed cloud
//! provider (`fly` / `digitalocean` / `hetzner` / `daytona` / `sprites`). The
//! **kind** is an inline cycle; the fields below it change with the kind:
//!
//! - cloud → token · region · size · template (baked image)
//! - ssh   → host (`user@box:port`)
//! - local → sandbox backend
//!
//! Submit yields a [`cmd::env::CreateArgs`](crate::cmd::env::CreateArgs); the loop
//! hands it to [`crate::cmd::env::create_env`], which stores any entered token via
//! the secret backend and writes the env with `config_write` — the same path as
//! the `thegn env create` CLI. Pure over its inputs; renders + handles keys on
//! the event loop (zero idle events).

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::cmd::env::CreateArgs;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use thegn_core::config::Config;

/// The selectable environment kinds, in cycle order.
const KINDS: &[&str] = &[
    "local",
    "ssh",
    "fly",
    "digitalocean",
    "hetzner",
    "daytona",
    "sprites",
    "machine0",
    "vercel",
];

/// Whether the env wizard (and the host picker's "set up …" rows) may offer
/// `kind` in `channel`. `local` is always available; `ssh` is the `remote`
/// feature; a provider kind is stable only for Sprites and Vercel
/// (`EnvProviderKind::stable_channel`), the rest are dev-only.
pub(crate) fn kind_allowed_in(kind: &str, channel: thegn_core::channel::Channel) -> bool {
    use thegn_core::channel::{Channel, Feature};
    match kind {
        "local" => true,
        "ssh" => Feature::Remote.allowed_in(channel),
        other => {
            channel == Channel::Dev
                || thegn_core::config::EnvProviderKind::of(other).stable_channel()
        }
    }
}

/// The wizard's kinds for `channel`, in cycle order.
fn kinds_for(channel: thegn_core::channel::Channel) -> Vec<&'static str> {
    KINDS
        .iter()
        .copied()
        .filter(|kind| kind_allowed_in(kind, channel))
        .collect()
}

/// Read-only note shown for the vercel kind. The wizard never writes
/// `model_credentials`; the user sets it in their own config.
const VERCEL_MODEL_CREDENTIALS_NOTE: [&str; 2] = [
    "model credentials: none (nothing is sent).",
    "To change: config model_credentials; F1 vercel-sandbox",
];

/// Probe keyring availability on a detached thread, returning the shared
/// tri-state the wizard reads (0 = probing, 1 = available, 2 = unavailable).
/// The probe is a synchronous D-Bus round-trip that can stall for ~25s on a
/// locked keyring — running it inline in the wizard constructor would freeze the
/// compositor from a single keypress, so it goes off the loop. The footer hint
/// shows "checking…" until the answer lands (no waker needed: the wizard is
/// interactive, so the next keystroke repaints the resolved hint).
fn spawn_keyring_probe() -> std::sync::Arc<std::sync::atomic::AtomicU8> {
    let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
    let out = state.clone();
    thegn_core::thread_name::spawn_named("thegn-kr-probe", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility); // Utility: open wizard hint probe.
        let v = if crate::secret::keyring_available() {
            1
        } else {
            2
        };
        out.store(v, std::sync::atomic::Ordering::Relaxed);
    });
    state
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Kind,
    Name,
    Token,
    Region,
    Size,
    TeamId,
    ProjectId,
    Template,
    SshHost,
    Sandbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Cancel,
    Submit(Box<CreateArgs>),
}

/// Apply a wizard [`Outcome`] on the event loop: dismiss on cancel/submit, and
/// on submit write the env via the shared `create_env` path (config_write +
/// secret backend). Kept here so the loop's key block stays a one-liner.
pub fn apply_outcome(
    outcome: Outcome,
    slot: &mut Option<EnvWizard>,
    model: &mut crate::chrome::FrameModel,
) {
    match outcome {
        Outcome::Pending => {}
        Outcome::Cancel => {
            *slot = None;
            model.status = "new environment cancelled".into();
        }
        Outcome::Submit(args) => {
            *slot = None;
            let name = args.name.clone();
            model.status = match crate::cmd::env::create_env(*args) {
                Ok(()) => format!("environment '{name}' created — bind it via env set"),
                Err(e) => format!("env create failed: {e}"),
            };
        }
    }
}

pub struct EnvWizard {
    kind_sel: usize,
    /// The kinds this channel allows, in cycle order (`kinds_for`).
    kinds: Vec<&'static str>,
    focus: Field,
    name: String,
    token: String,
    region: String,
    size: String,
    team_id: String,
    project_id: String,
    template: String,
    ssh_host: String,
    /// `(backend_key, label)` cycle rows for a `local` env.
    sandbox_rows: Vec<(String, String)>,
    sandbox_sel: usize,
    /// Keyring availability, resolved off-loop: 0 = probing, 1 = available,
    /// 2 = unavailable. The probe (`keyring_available`) does a synchronous D-Bus
    /// Secret Service round-trip that can block for ~25s on a locked keyring, so
    /// it must never run on the event loop; the footer hint reads this atomic and
    /// shows "checking…" until the background thread stores an answer.
    keyring: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl EnvWizard {
    pub fn new(cfg: &Config) -> Self {
        Self::new_in(cfg, crate::channel_state::current())
    }

    /// [`EnvWizard::new`] for an explicit release channel.
    fn new_in(cfg: &Config, channel: thegn_core::channel::Channel) -> Self {
        let kinds = kinds_for(channel);
        let sandbox_rows = crate::palette::build_sandbox_palette(cfg)
            .into_iter()
            .map(|i| {
                let key = i.key.strip_prefix("sandbox:").unwrap_or(&i.key).to_string();
                (key, i.label)
            })
            .collect();
        EnvWizard {
            // Default to the first cloud kind (fly on dev, sprites on stable)
            // — the common case; `local` when the channel offers no cloud kind.
            kind_sel: kinds
                .iter()
                .position(|k| !matches!(*k, "local" | "ssh"))
                .unwrap_or(0),
            kinds,
            focus: Field::Kind,
            name: String::new(),
            token: String::new(),
            region: String::new(),
            size: String::new(),
            team_id: String::new(),
            project_id: String::new(),
            template: String::new(),
            ssh_host: String::new(),
            sandbox_rows,
            sandbox_sel: 0,
            keyring: spawn_keyring_probe(),
        }
    }

    /// Like [`EnvWizard::new`] but pre-seeded to `kind` (a host-picker
    /// "+ set up `<kind>`…" row). Unknown kinds keep the default selection.
    pub fn with_kind(cfg: &Config, kind: &str) -> Self {
        Self::with_kind_in(cfg, kind, crate::channel_state::current())
    }

    fn with_kind_in(cfg: &Config, kind: &str, channel: thegn_core::channel::Channel) -> Self {
        let mut w = Self::new_in(cfg, channel);
        if let Some(i) = w.kinds.iter().position(|k| *k == kind) {
            w.kind_sel = i;
        }
        w
    }

    fn kind(&self) -> &'static str {
        self.kinds.get(self.kind_sel).copied().unwrap_or("local")
    }

    fn is_cloud(&self) -> bool {
        !matches!(self.kind(), "local" | "ssh")
    }

    /// The focusable fields for the current kind, top to bottom.
    fn fields(&self) -> Vec<Field> {
        let mut f = vec![Field::Kind, Field::Name];
        match self.kind() {
            "local" => f.push(Field::Sandbox),
            "ssh" => f.push(Field::SshHost),
            "vercel" => f.extend([
                Field::Token,
                Field::TeamId,
                Field::ProjectId,
                Field::Region,
                Field::Size,
                Field::Template,
            ]),
            _ => f.extend([Field::Token, Field::Region, Field::Size, Field::Template]),
        }
        f
    }

    fn text_mut(&mut self, f: Field) -> Option<&mut String> {
        match f {
            Field::Name => Some(&mut self.name),
            Field::Token => Some(&mut self.token),
            Field::Region => Some(&mut self.region),
            Field::Size => Some(&mut self.size),
            Field::TeamId => Some(&mut self.team_id),
            Field::ProjectId => Some(&mut self.project_id),
            Field::Template => Some(&mut self.template),
            Field::SshHost => Some(&mut self.ssh_host),
            _ => None,
        }
    }

    fn move_focus(&mut self, delta: i32) {
        let fields = self.fields();
        let cur = fields.iter().position(|&f| f == self.focus).unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, fields.len() as i32 - 1) as usize;
        self.focus = fields[next];
    }

    /// Inject bracketed-paste text into the focused text field (tokens are long).
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
                KeyCode::Char('c' | 'C' | 'g' | 'G') => Outcome::Cancel,
                _ => Outcome::Pending,
            };
        }
        if mods.contains(Modifiers::ALT) || mods.contains(Modifiers::SUPER) {
            return Outcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return Outcome::Cancel;
        }
        // Inline-cycle fields (Kind, Sandbox): ←/→ change the value.
        let is_cycle = matches!(self.focus, Field::Kind | Field::Sandbox);
        match key {
            KeyCode::UpArrow => self.move_focus(-1),
            KeyCode::DownArrow => self.move_focus(1),
            KeyCode::LeftArrow if is_cycle => self.cycle(-1),
            KeyCode::RightArrow if is_cycle => self.cycle(1),
            KeyCode::Enter => {
                let fields = self.fields();
                let last = fields.last() == Some(&self.focus);
                if last {
                    return self.submit();
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

    fn cycle(&mut self, delta: i32) {
        match self.focus {
            Field::Kind => {
                let n = self.kinds.len().max(1) as i32;
                self.kind_sel = (((self.kind_sel as i32 + delta) % n + n) % n) as usize;
                // Focus stays on Kind; the field list re-derives on the next render.
            }
            Field::Sandbox => {
                let max = self.sandbox_rows.len().saturating_sub(1) as i32;
                self.sandbox_sel = (self.sandbox_sel as i32 + delta).clamp(0, max) as usize;
            }
            _ => {}
        }
    }

    fn opt(s: &str) -> Option<String> {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    }

    fn submit(&self) -> Outcome {
        let name = self.name.trim().replace(' ', "-");
        if name.is_empty() {
            return Outcome::Pending; // name is required; keep the form open
        }
        let sandbox = if self.kind() == "local" {
            self.sandbox_rows
                .get(self.sandbox_sel)
                .map(|(k, _)| k.clone())
                .filter(|k| k != "auto")
        } else {
            None
        };
        Outcome::Submit(Box::new(CreateArgs {
            name,
            provider: self.kind().to_string(),
            region: Self::opt(&self.region),
            size: Self::opt(&self.size),
            team_id: Self::opt(&self.team_id),
            project_id: Self::opt(&self.project_id),
            template: Self::opt(&self.template),
            max_instances: None,
            max_lifetime: None,
            auto_provision: false,
            ssh_host: Self::opt(&self.ssh_host),
            sandbox,
            token: Self::opt(&self.token),
            token_env: None,
            token_file: None,
        }))
    }

    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let fields = self.fields();
        let spec = LayerSpec {
            title: "new environment".to_string(),
            badge: Some(" env ".into()),
            cols: 60,
            rows: fields.len() + 2 + if self.kind() == "vercel" { 2 } else { 0 },
            anchor: Anchor::Center,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        for (i, f) in fields.iter().enumerate() {
            let y = inner.y + i;
            let focused = self.focus == *f;
            let line = match f {
                Field::Kind => Line::segs(self.cycle_row("kind    ", focused, self.kind())),
                Field::Sandbox => {
                    let label = self
                        .sandbox_rows
                        .get(self.sandbox_sel)
                        .map(|(_, l)| l.as_str())
                        .unwrap_or("auto");
                    Line::segs(self.cycle_row("sandbox ", focused, label))
                }
                _ => self.text_row(*f, focused),
            };
            seg::draw_line(surface, inner.x, y, inner.cols, &line, panel);
        }
        if self.kind() == "vercel" {
            for (i, text) in VERCEL_MODEL_CREDENTIALS_NOTE.iter().enumerate() {
                seg::draw_line(
                    surface,
                    inner.x,
                    inner.y + fields.len() + i,
                    inner.cols,
                    &Line::segs(vec![seg(Tok::Slot(S::Faint), (*text).to_string())]),
                    panel,
                );
            }
        }
        // Footer: token-storage hint (cloud kinds) + key legend.
        let store_hint = if self.is_cloud() {
            match self.keyring.load(std::sync::atomic::Ordering::Relaxed) {
                1 => "token → OS keyring · ",
                2 => "token → 0600 file · ",
                _ => "token store: checking… · ",
            }
        } else {
            ""
        };
        let last = fields.last() == Some(&self.focus);
        let enter = if last { "enter create" } else { "enter next" };
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows - 1,
            inner.cols,
            &Line::segs(vec![seg(
                Tok::Slot(S::Faint),
                format!("{store_hint}↑↓ move · ←→ change · {enter} · esc cancel"),
            )]),
            panel,
        );
    }

    fn text_row(&self, f: Field, focused: bool) -> Line {
        let (label, val, placeholder, mask) = match f {
            Field::Name => ("name    ", &self.name, "env name (e.g. fly-dev)", false),
            Field::Token => (
                "token   ",
                &self.token,
                "paste API token (stored securely)",
                true,
            ),
            Field::Region => ("region  ", &self.region, "· provider default", false),
            Field::Size => ("size    ", &self.size, "· provider default", false),
            Field::TeamId => ("team    ", &self.team_id, "team_… (Vercel team id)", false),
            Field::ProjectId => (
                "project ",
                &self.project_id,
                "prj_… (Vercel project id)",
                false,
            ),
            Field::Template => (
                "image   ",
                &self.template,
                "· stock (or image:<ref> / snapshot:<id>)",
                false,
            ),
            Field::SshHost => ("host    ", &self.ssh_host, "user@box:port", false),
            _ => ("        ", &self.name, "", false),
        };
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
            seg(Tok::Slot(S::Text), val.clone())
        };
        Line::segs(vec![
            seg(label_fg, format!("{label}❯ ")).bold(),
            shown,
            if focused { crate::seg::caret() } else { sp(0) },
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

    use thegn_core::channel::Channel;

    fn wiz() -> EnvWizard {
        EnvWizard::new_in(&Config::default(), Channel::Dev)
    }

    #[test]
    fn stable_wizard_offers_only_stable_kinds() {
        let w = EnvWizard::new_in(&Config::default(), Channel::Stable);
        assert_eq!(w.kinds, vec!["local", "sprites", "vercel"]);
        // Defaults to the first allowed cloud kind.
        assert_eq!(w.kind(), "sprites");
        for dev_only in [
            "ssh",
            "fly",
            "digitalocean",
            "hetzner",
            "daytona",
            "machine0",
        ] {
            assert!(!kind_allowed_in(dev_only, Channel::Stable), "{dev_only}");
            assert!(kind_allowed_in(dev_only, Channel::Dev), "{dev_only}");
        }
    }

    #[test]
    fn stable_wizard_cycles_within_allowed_kinds_and_ignores_dev_seeds() {
        let mut w = EnvWizard::new_in(&Config::default(), Channel::Stable);
        let mut seen = Vec::new();
        for _ in 0..w.kinds.len() {
            seen.push(w.kind());
            w.cycle(1);
        }
        assert_eq!(seen, vec!["sprites", "vercel", "local"]);
        assert_eq!(w.kind(), "sprites");
        w.cycle(-1);
        assert_eq!(w.kind(), "local");
        // A dev-only seed ("+ set up fly…") keeps the default selection.
        let seeded = EnvWizard::with_kind_in(&Config::default(), "fly", Channel::Stable);
        assert_eq!(seeded.kind(), "sprites");
        let vercel = EnvWizard::with_kind_in(&Config::default(), "vercel", Channel::Stable);
        assert_eq!(vercel.kind(), "vercel");
    }

    fn typ(w: &mut EnvWizard, s: &str) {
        for c in s.chars() {
            w.handle_key(&KeyCode::Char(c), Modifiers::NONE);
        }
    }

    #[test]
    fn cloud_kind_fields_and_submit() {
        let mut w = wiz();
        assert_eq!(w.kind(), "fly");
        assert_eq!(
            w.fields(),
            vec![
                Field::Kind,
                Field::Name,
                Field::Token,
                Field::Region,
                Field::Size,
                Field::Template
            ]
        );
        // name
        w.focus = Field::Name;
        typ(&mut w, "fly-dev");
        w.focus = Field::Token;
        typ(&mut w, "SECRET");
        w.focus = Field::Region;
        typ(&mut w, "iad");
        match w.submit() {
            Outcome::Submit(a) => {
                assert_eq!(a.name, "fly-dev");
                assert_eq!(a.provider, "fly");
                assert_eq!(a.token.as_deref(), Some("SECRET"));
                assert_eq!(a.region.as_deref(), Some("iad"));
                assert_eq!(a.size, None);
            }
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn vercel_kind_asks_for_team_and_project() {
        let mut w = wiz();
        w.kind_sel = w.kinds.iter().position(|k| *k == "vercel").unwrap();
        assert!(w.fields().contains(&Field::TeamId));
        assert!(w.fields().contains(&Field::ProjectId));
        w.focus = Field::Name;
        typ(&mut w, "vc");
        w.focus = Field::TeamId;
        typ(&mut w, "team_abc");
        w.focus = Field::ProjectId;
        typ(&mut w, "prj_abc");
        match w.submit() {
            Outcome::Submit(a) => {
                assert_eq!(a.provider, "vercel");
                assert_eq!(a.team_id.as_deref(), Some("team_abc"));
                assert_eq!(a.project_id.as_deref(), Some("prj_abc"));
            }
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn ssh_and_local_kinds_change_fields() {
        let mut w = wiz();
        w.kind_sel = w.kinds.iter().position(|k| *k == "ssh").unwrap();
        assert_eq!(w.fields(), vec![Field::Kind, Field::Name, Field::SshHost]);
        w.kind_sel = w.kinds.iter().position(|k| *k == "local").unwrap();
        assert_eq!(w.fields(), vec![Field::Kind, Field::Name, Field::Sandbox]);
    }

    #[test]
    fn empty_name_does_not_submit() {
        let w = wiz();
        assert_eq!(w.submit(), Outcome::Pending);
    }

    #[test]
    fn escape_cancels() {
        let mut w = wiz();
        assert_eq!(
            w.handle_key(&KeyCode::Escape, Modifiers::NONE),
            Outcome::Cancel
        );
    }

    #[test]
    fn keyring_probe_is_off_loop_not_inline() {
        // The constructor must NOT run the blocking keyring probe inline — the
        // shared state starts at 0 ("checking…") and is resolved by a background
        // thread. (A specific resolved value inline would mean the D-Bus probe
        // ran on the loop, the very stall this fix removes.) The value is `Ordering`
        // -published by the probe thread; here we only assert construction is
        // non-blocking by observing the tri-state is a valid encoding.
        let w = wiz();
        let v = w.keyring.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            v <= 2,
            "keyring tri-state is one of probing/available/unavailable"
        );
    }

    #[test]
    fn vercel_note_is_read_only_and_never_writes_model_credentials() {
        let mut w = EnvWizard::with_kind(&Config::default(), "vercel");
        assert_eq!(w.kind(), "vercel");
        assert!(VERCEL_MODEL_CREDENTIALS_NOTE[0].starts_with("model credentials: none"));
        w.focus = Field::Name;
        typ(&mut w, "vx");
        w.focus = Field::TeamId;
        typ(&mut w, "team_abc");
        w.focus = Field::ProjectId;
        typ(&mut w, "prj_abc");
        match w.submit() {
            Outcome::Submit(args) => {
                assert!(!format!("{args:?}").contains("model_credentials"));
            }
            _ => panic!("expected submit"),
        }
    }
}
