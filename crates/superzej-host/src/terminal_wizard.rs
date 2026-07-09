//! The new-terminal wizard (`Alt T` / command palette → "New terminal…").
//!
//! A compact, single-plane modal mirroring the new-worktree wizard
//! ([`crate::wizard::NewWorktreeWizard`]) but without a background worker —
//! creating a terminal is a synchronous DB insert + pane spawn, so the loop
//! handles submit inline. The form collects:
//!
//! - **name** — free text; when left untouched it defaults to a random
//!   `adj-noun` slug (e.g. `snappy-shark`). For a remote target the selected
//!   host is prefixed as context (`<host>/<slug>`, e.g. `build-box/snappy-shark`);
//!   a local shell shows just the slug. Typing overrides the default verbatim.
//! - **host** — an inline cycle over the machines registered in `[host.*]`
//!   (reach `ssh`/`local`; iroh/cloud have no interactive pane transport):
//!   `local` = a local shell, a registered host prefills the connection from
//!   its config, and `manual…` reveals the free-text **connection** field for an
//!   arbitrary `ssh user@host` / `mosh user@host` target.
//! - **connection** — free text, shown only in `manual…` mode: empty = a local
//!   shell; otherwise a remote terminal.
//! - **sandbox** — an inline cycle over the configured backends, applied only to
//!   a *local* shell (a remote terminal's isolation is owned by the remote end,
//!   so the field reads "managed by remote" and is skipped for focus).
//!
//! The whole struct is pure over its inputs; rendering + key handling run on the
//! event loop, so an idle superzej still produces zero events.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use superzej_core::config::Config;
use superzej_core::host_config::{HostConfig, HostReach};

/// Which field of the single-plane form has focus, top-to-bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Host,
    Connection,
    Sandbox,
}

/// What a key delivered to the wizard meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Cancel,
    Submit(TerminalChoice),
}

/// The resolved form on submit — everything the loop needs to create + spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalChoice {
    /// Unique terminal name (also the sidebar/session group name).
    pub name: String,
    /// `"local"` for a shell, `"remote"` for ssh/mosh.
    pub kind: String,
    /// Connection command (`""` = local shell; `ssh user@host` = remote).
    pub connection: String,
    /// Sandbox backend label for a local shell (`""`/`host` = uncontained);
    /// always empty for a remote terminal.
    pub sandbox: String,
}

/// One entry in the `host` cycle: `local`, a registered `[host.*]`, or `manual…`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostChoice {
    /// Short display label (`local shell`, `<name> · ssh`, `manual…`).
    label: String,
    /// Resolved connection string for a registered host (`""` for `local`;
    /// unused for `manual`, which reads the free-text field).
    connection: String,
    /// Bare host slug used as name context for a remote terminal (the registered
    /// host's key, e.g. `build-box`); `""` for `local` and `manual`.
    host_slug: String,
    /// The `manual…` sentinel — focus flows to the free-text connection field.
    manual: bool,
}

pub struct TerminalWizard {
    focus: Field,
    name: String,
    name_edited: bool,
    /// The random `adj-noun` slug used as the default terminal name, fixed at
    /// construction (deduped against existing terminals) so the preview is
    /// stable across frames.
    generated_slug: String,
    connection: String,
    /// `local` first, the registered hosts, then `manual…` last.
    host_rows: Vec<HostChoice>,
    host_sel: usize,
    /// `(backend_key, label)` cycle rows, e.g. `("bwrap", "▣ Bubblewrap")`.
    sandbox_rows: Vec<(String, String)>,
    sandbox_sel: usize,
}

/// The connection string for a registered host, or `None` when its reach has no
/// interactive pane transport (iroh/cloud — they provision but don't back a
/// terminal). Terminals always speak plain `ssh` (universally available and the
/// pane parser understands `ssh [-p N] target`); the host's `transport`
/// preference governs long-lived sandbox panes, not a quick shell.
fn host_connection(name: &str, hc: &HostConfig) -> Option<String> {
    match hc.reach {
        HostReach::Local => Some(String::new()),
        HostReach::Ssh => {
            let target = hc.ssh.host.trim();
            let target = if target.is_empty() { name } else { target };
            let mut conn = String::from("ssh ");
            if hc.ssh.port != 0 && hc.ssh.port != 22 {
                conn.push_str(&format!("-p {} ", hc.ssh.port));
            }
            conn.push_str(target);
            Some(conn)
        }
        HostReach::Iroh | HostReach::Cloud => None,
    }
}

impl TerminalWizard {
    /// `taken` is the set of existing terminal names, used to dedupe the random
    /// default slug so back-to-back creates don't collide.
    pub fn new(cfg: &Config, taken: &[String]) -> Self {
        let sandbox_rows = crate::palette::build_sandbox_palette(cfg)
            .into_iter()
            .map(|i| {
                let key = i.key.strip_prefix("sandbox:").unwrap_or(&i.key).to_string();
                (key, i.label)
            })
            .collect();
        // `local` first, the terminal-capable registered hosts (sorted for a
        // stable cycle), then `manual…`.
        let mut host_rows = vec![HostChoice {
            label: "local shell".into(),
            connection: String::new(),
            host_slug: String::new(),
            manual: false,
        }];
        let mut names: Vec<&String> = cfg.host.keys().collect();
        names.sort();
        for name in names {
            let hc = &cfg.host[name];
            let Some(connection) = host_connection(name, hc) else {
                continue; // iroh/cloud: no interactive pane transport
            };
            let reach = match hc.reach {
                HostReach::Ssh => "ssh",
                HostReach::Local => "local",
                HostReach::Iroh => "iroh",
                HostReach::Cloud => "cloud",
            };
            host_rows.push(HostChoice {
                label: format!("{name} · {reach}"),
                connection,
                host_slug: name.clone(),
                manual: false,
            });
        }
        host_rows.push(HostChoice {
            label: "manual…".into(),
            connection: String::new(),
            host_slug: String::new(),
            manual: true,
        });
        // A random `adj-noun` default, deduped against existing terminal names.
        let taken_set = superzej_core::worktree::BranchSet::from_names(taken.iter().cloned());
        let generated_slug =
            superzej_core::worktree::dedupe(&superzej_core::worktree::random_pair(), &taken_set);
        TerminalWizard {
            focus: Field::Name,
            name: String::new(),
            name_edited: false,
            generated_slug,
            connection: String::new(),
            host_rows,
            host_sel: 0,
            sandbox_rows,
            sandbox_sel: 0,
        }
    }

    fn selected_host(&self) -> &HostChoice {
        // host_sel is always kept in-bounds; `local` is a safe fallback.
        self.host_rows
            .get(self.host_sel)
            .unwrap_or(&self.host_rows[0])
    }

    /// `manual…` is selected — the free-text connection field is live.
    fn is_manual(&self) -> bool {
        self.selected_host().manual
    }

    /// The connection this form resolves to: the free-text field in manual mode,
    /// else the selected host's derived string.
    fn effective_connection(&self) -> String {
        if self.is_manual() {
            self.connection.trim().to_string()
        } else {
            self.selected_host().connection.clone()
        }
    }

    /// A local shell (no connection) — the only case where the sandbox applies.
    fn is_local(&self) -> bool {
        self.effective_connection().is_empty()
    }

    /// The host slug used as name context, or `None` for a local shell. For a
    /// registered host it is the host key; for a manual remote it is the
    /// sanitized ssh target (last whitespace-separated token).
    fn host_token(&self) -> Option<String> {
        let hc = self.selected_host();
        if hc.manual {
            let conn = self.connection.trim();
            if conn.is_empty() {
                return None; // manual local shell
            }
            // Strip a leading ssh/mosh verb (+ any flags), then sanitize the
            // target into a slug (the target is the last token).
            let rest = conn
                .strip_prefix("ssh ")
                .or_else(|| conn.strip_prefix("mosh "))
                .unwrap_or(conn)
                .trim();
            let target = rest.split_whitespace().last().unwrap_or(rest);
            return Some(target.replace([' ', '@', ':', '/'], "-"));
        }
        if hc.connection.trim().is_empty() {
            return None; // local shell
        }
        Some(hc.host_slug.clone())
    }

    /// The name to persist: the typed name if the user edited it, else the
    /// random slug (`snappy-shark`), prefixed with the host as context for a
    /// remote target (`<host>/snappy-shark`).
    fn resolved_name(&self) -> String {
        let typed = self.name.trim();
        if self.name_edited && !typed.is_empty() {
            return typed.replace(' ', "-");
        }
        match self.host_token() {
            Some(host) => format!("{host}/{}", self.generated_slug),
            None => self.generated_slug.clone(),
        }
    }

    fn sandbox_key(&self) -> &str {
        self.sandbox_rows
            .get(self.sandbox_sel)
            .map(|(k, _)| k.as_str())
            .unwrap_or("auto")
    }

    /// Where focus lands after the Host row: the free-text connection in manual
    /// mode; the sandbox for a local shell; nowhere (submit on Enter) otherwise.
    fn focus_after_host(&mut self) {
        self.focus = if self.is_manual() {
            Field::Connection
        } else if self.is_local() {
            Field::Sandbox
        } else {
            // A registered remote host: nothing more to fill — Enter submits.
            Field::Host
        };
    }

    fn focus_down(&mut self) {
        match self.focus {
            Field::Name => self.focus = Field::Host,
            Field::Host => self.focus_after_host(),
            // Skip Sandbox for a remote (manual) target — host-managed there.
            Field::Connection if self.is_local() => self.focus = Field::Sandbox,
            Field::Connection => {}
            Field::Sandbox => {}
        }
    }

    fn focus_up(&mut self) {
        self.focus = match self.focus {
            // Sandbox sits below Connection (manual) or Host (a local host pick).
            Field::Sandbox if self.is_manual() => Field::Connection,
            Field::Sandbox => Field::Host,
            Field::Connection => Field::Host,
            Field::Host => Field::Name,
            Field::Name => Field::Name,
        };
    }

    /// Inject pasted text into the focused text field (Name or Connection).
    pub fn handle_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        match self.focus {
            Field::Name => {
                self.name.push_str(&clean);
                self.name_edited = true;
            }
            Field::Connection => self.connection.push_str(&clean),
            Field::Host | Field::Sandbox => {}
        }
    }

    fn cycle_host(&mut self, delta: isize) {
        let max = self.host_rows.len().saturating_sub(1);
        let next = (self.host_sel as isize + delta).clamp(0, max as isize);
        self.host_sel = next as usize;
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
        match self.focus {
            Field::Name => {
                match key {
                    KeyCode::Enter | KeyCode::DownArrow => self.focus_down(),
                    KeyCode::UpArrow => self.focus_up(),
                    KeyCode::Backspace => {
                        self.name_edited |= self.name.pop().is_some();
                    }
                    KeyCode::Char(c) => {
                        self.name.push(*c);
                        self.name_edited = true;
                    }
                    _ => {}
                }
                Outcome::Pending
            }
            // Inline cycle over local / hosts / manual…. Enter advances (or, for
            // a ready-to-go remote host, submits); ←/→ change the selection.
            Field::Host => match key {
                KeyCode::LeftArrow => {
                    self.cycle_host(-1);
                    Outcome::Pending
                }
                KeyCode::RightArrow => {
                    self.cycle_host(1);
                    Outcome::Pending
                }
                KeyCode::UpArrow => {
                    self.focus_up();
                    Outcome::Pending
                }
                KeyCode::Enter | KeyCode::DownArrow => {
                    // A registered remote host has nothing left to fill → submit.
                    if !self.is_manual() && !self.is_local() {
                        return self.submit();
                    }
                    self.focus_down();
                    Outcome::Pending
                }
                _ => Outcome::Pending,
            },
            // Connection is a free-text field (manual mode); Enter submits when
            // the terminal is remote (no Sandbox step), else advances to Sandbox.
            Field::Connection => {
                match key {
                    KeyCode::Enter => {
                        if self.is_local() {
                            self.focus_down();
                        } else {
                            return self.submit();
                        }
                    }
                    KeyCode::DownArrow => self.focus_down(),
                    KeyCode::UpArrow => self.focus_up(),
                    KeyCode::Backspace => {
                        self.connection.pop();
                    }
                    KeyCode::Char(c) => self.connection.push(*c),
                    _ => {}
                }
                Outcome::Pending
            }
            // Inline cycle: ←/→ change the backend; ↑ moves focus; Enter submits.
            Field::Sandbox => match key {
                KeyCode::LeftArrow => {
                    self.sandbox_sel = self.sandbox_sel.saturating_sub(1);
                    Outcome::Pending
                }
                KeyCode::RightArrow => {
                    let max = self.sandbox_rows.len().saturating_sub(1);
                    self.sandbox_sel = (self.sandbox_sel + 1).min(max);
                    Outcome::Pending
                }
                KeyCode::UpArrow => {
                    self.focus_up();
                    Outcome::Pending
                }
                KeyCode::Enter => self.submit(),
                _ => Outcome::Pending,
            },
        }
    }

    fn submit(&self) -> Outcome {
        let connection = self.effective_connection();
        let local = connection.is_empty();
        Outcome::Submit(TerminalChoice {
            name: self.resolved_name(),
            kind: if local { "local" } else { "remote" }.to_string(),
            connection,
            // A remote terminal is isolated by the remote end; only a local
            // shell carries a host-side sandbox backend.
            sandbox: if local {
                self.sandbox_key().to_string()
            } else {
                String::new()
            },
        })
    }

    /// Paint the single-plane form as a centered layer.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        // Rows: name + host are always shown; connection only in manual mode;
        // sandbox only for a local shell. Plus a footer.
        let show_conn = self.is_manual();
        let show_sandbox = self.is_local();
        let body_rows = 2 + usize::from(show_conn) + usize::from(show_sandbox);
        let spec = LayerSpec {
            title: "new terminal".to_string(),
            badge: Some(" Alt+T ".into()),
            cols: 54,
            rows: body_rows + 2,
            anchor: Anchor::Center,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        let label_fg = |focused: bool| {
            if focused {
                Tok::Slot(S::Accent)
            } else {
                Tok::Slot(S::Faint)
            }
        };
        let mut y = inner.y;

        // --- name (editable) ----------------------------------------------
        let name_focused = self.focus == Field::Name;
        let shown_name = if self.name_edited {
            self.name.clone()
        } else {
            self.resolved_name()
        };
        seg::draw_line(
            surface,
            inner.x,
            y,
            inner.cols,
            &Line::segs(vec![
                seg(label_fg(name_focused), "name    ❯ ".to_string()).bold(),
                seg(
                    if self.name_edited {
                        Tok::Slot(S::Text)
                    } else {
                        Tok::Slot(S::Faint)
                    },
                    shown_name,
                ),
                if name_focused {
                    seg(Tok::Slot(S::Accent), "▏")
                } else {
                    sp(0)
                },
            ]),
            panel,
        );
        y += 1;

        // --- host (inline cycle: local / registered / manual…) ------------
        let host_focused = self.focus == Field::Host;
        seg::draw_line(
            surface,
            inner.x,
            y,
            inner.cols,
            &Line::segs(self.cycle_row("host", host_focused, &self.selected_host().label)),
            panel,
        );
        y += 1;

        // --- connection (editable; manual mode only) ----------------------
        if show_conn {
            let conn_focused = self.focus == Field::Connection;
            let conn_shown = if self.connection.is_empty() {
                seg(
                    Tok::Slot(S::Faint),
                    "· local shell (ssh user@host for remote)",
                )
            } else {
                seg(Tok::Slot(S::Text), self.connection.clone())
            };
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![
                    seg(label_fg(conn_focused), "connect ❯ ".to_string()).bold(),
                    conn_shown,
                    if conn_focused {
                        seg(Tok::Slot(S::Accent), "▏")
                    } else {
                        sp(0)
                    },
                ]),
                panel,
            );
            y += 1;
        }

        // --- sandbox (inline cycle; local shells only) --------------------
        if show_sandbox {
            let sb_focused = self.focus == Field::Sandbox;
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
        }

        // Footer hint.
        let enter_verb = if self.focus == Field::Name {
            "enter next"
        } else if self.focus == Field::Host && !self.is_manual() && !self.is_local() {
            "enter create"
        } else {
            "enter next/create"
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

    /// A `label ‹ value ›` inline choice row; chevrons appear only when focused.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn wiz() -> TerminalWizard {
        TerminalWizard::new(&Config::default(), &[])
    }

    /// A config with a couple of registered hosts (ssh + iroh) plus a
    /// non-default-port ssh host, to exercise the host cycle.
    fn cfg_with_hosts() -> Config {
        use superzej_core::host_config::HostConfig;
        let mut cfg = Config::default();
        cfg.host.insert(
            "build-box".into(),
            HostConfig {
                reach: HostReach::Ssh,
                ssh: superzej_core::config::EnvSshConfig {
                    host: "dev@builder".into(),
                    port: 22,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        cfg.host.insert(
            "ported".into(),
            HostConfig {
                reach: HostReach::Ssh,
                ssh: superzej_core::config::EnvSshConfig {
                    host: "me@far".into(),
                    port: 2222,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        // iroh has no interactive pane transport → excluded from the cycle.
        cfg.host.insert(
            "pi".into(),
            HostConfig {
                reach: HostReach::Iroh,
                ..Default::default()
            },
        );
        cfg
    }

    #[test]
    fn local_shell_derives_name_and_carries_sandbox() {
        let mut w = wiz();
        // Force focus onto the (local-shell) Sandbox field with a known backend.
        w.focus = Field::Sandbox;
        w.sandbox_rows = vec![("bwrap".into(), "▣ Bubblewrap".into())];
        w.sandbox_sel = 0;
        match w.submit() {
            Outcome::Submit(c) => {
                // A local shell now defaults to the random slug (no host prefix).
                assert_eq!(c.name, w.generated_slug);
                assert!(c.name.contains('-'), "adj-noun slug: {}", c.name);
                assert!(!c.name.contains('/'), "local carries no host prefix");
                assert_eq!(c.kind, "local");
                assert_eq!(c.connection, "");
                assert_eq!(c.sandbox, "bwrap");
            }
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn manual_remote_connection_derives_name_and_drops_sandbox() {
        let mut w = wiz();
        // Pick `manual…` (last cycle entry), then type a remote target.
        w.host_sel = w.host_rows.len() - 1;
        assert!(w.is_manual());
        for c in "ssh user@host".chars() {
            w.connection.push(c);
        }
        match w.submit() {
            Outcome::Submit(c) => {
                assert_eq!(c.kind, "remote");
                assert_eq!(c.connection, "ssh user@host");
                // Host context (sanitized ssh target) prefixes the random slug.
                assert_eq!(c.name, format!("user-host/{}", w.generated_slug));
                assert_eq!(c.sandbox, "", "remote terminals carry no host sandbox");
            }
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn registered_ssh_host_resolves_connection_and_port() {
        let mut w = TerminalWizard::new(&cfg_with_hosts(), &[]);
        // Cycle: [local, build-box, ported, manual…] — iroh `pi` excluded.
        assert_eq!(w.host_rows.len(), 4);
        assert_eq!(w.host_rows[0].label, "local shell");
        assert_eq!(w.host_rows[1].label, "build-box · ssh");
        assert_eq!(w.host_rows[2].label, "ported · ssh");
        assert!(w.host_rows[3].manual);

        // A default-port ssh host → `ssh <target>`, remote, no sandbox.
        w.host_sel = 1;
        match w.submit() {
            Outcome::Submit(c) => {
                assert_eq!(c.connection, "ssh dev@builder");
                assert_eq!(c.kind, "remote");
                // Host context is the host *key* (`build-box`), not the target.
                assert_eq!(c.name, format!("build-box/{}", w.generated_slug));
            }
            _ => panic!("expected submit"),
        }

        // A non-default port carries `-p N`.
        w.host_sel = 2;
        match w.submit() {
            Outcome::Submit(c) => assert_eq!(c.connection, "ssh -p 2222 me@far"),
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn host_cycle_enter_submits_a_ready_remote_host() {
        let mut w = TerminalWizard::new(&cfg_with_hosts(), &[]);
        w.focus = Field::Host;
        w.host_sel = 1; // build-box (remote, nothing left to fill)
        match w.handle_key(&KeyCode::Enter, Modifiers::NONE) {
            Outcome::Submit(c) => assert_eq!(c.connection, "ssh dev@builder"),
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn local_host_pick_advances_to_sandbox() {
        let mut w = TerminalWizard::new(&cfg_with_hosts(), &[]);
        w.focus = Field::Host;
        w.host_sel = 0; // local shell
        assert_eq!(
            w.handle_key(&KeyCode::Enter, Modifiers::NONE),
            Outcome::Pending
        );
        assert_eq!(w.focus, Field::Sandbox);
    }

    #[test]
    fn manual_pick_reveals_connection_field() {
        let mut w = TerminalWizard::new(&cfg_with_hosts(), &[]);
        w.focus = Field::Host;
        w.host_sel = w.host_rows.len() - 1; // manual…
        assert_eq!(
            w.handle_key(&KeyCode::Enter, Modifiers::NONE),
            Outcome::Pending
        );
        assert_eq!(w.focus, Field::Connection);
    }

    #[test]
    fn typed_name_wins_over_derivation() {
        let mut w = wiz();
        for c in "prod-box".chars() {
            w.handle_key(&KeyCode::Char(c), Modifiers::NONE);
        }
        assert!(w.name_edited);
        assert_eq!(w.resolved_name(), "prod-box");
    }

    #[test]
    fn escape_cancels() {
        let mut w = wiz();
        assert_eq!(
            w.handle_key(&KeyCode::Escape, Modifiers::NONE),
            Outcome::Cancel
        );
    }
}
