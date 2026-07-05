//! The new-terminal wizard (`Alt T` / command palette → "New terminal…").
//!
//! A compact, single-plane modal mirroring the new-worktree wizard
//! ([`crate::wizard::NewWorktreeWizard`]) but without a background worker —
//! creating a terminal is a synchronous DB insert + pane spawn, so the loop
//! handles submit inline. The form collects three fields:
//!
//! - **name** — free text; when left untouched it is derived from the
//!   connection (`local` for a local shell, `term-<host>` for a remote target).
//! - **connection** — free text: empty = a local shell; `ssh user@host`,
//!   `mosh user@host`, or a bare `user@host` = a remote terminal.
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

/// Which field of the single-plane form has focus, top-to-bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
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

pub struct TerminalWizard {
    focus: Field,
    name: String,
    name_edited: bool,
    connection: String,
    /// `(backend_key, label)` cycle rows, e.g. `("bwrap", "▣ Bubblewrap")`.
    sandbox_rows: Vec<(String, String)>,
    sandbox_sel: usize,
}

impl TerminalWizard {
    pub fn new(cfg: &Config) -> Self {
        let sandbox_rows = crate::palette::build_sandbox_palette(cfg)
            .into_iter()
            .map(|i| {
                let key = i.key.strip_prefix("sandbox:").unwrap_or(&i.key).to_string();
                (key, i.label)
            })
            .collect();
        TerminalWizard {
            focus: Field::Name,
            name: String::new(),
            name_edited: false,
            connection: String::new(),
            sandbox_rows,
            sandbox_sel: 0,
        }
    }

    /// A local shell (no connection) — the only case where the sandbox applies.
    fn is_local(&self) -> bool {
        self.connection.trim().is_empty()
    }

    /// The name to persist: the typed name if the user edited it, else one
    /// derived from the connection (`local`, or `term-<sanitized-host>`).
    fn resolved_name(&self) -> String {
        let typed = self.name.trim();
        if self.name_edited && !typed.is_empty() {
            return typed.replace(' ', "-");
        }
        let conn = self.connection.trim();
        if conn.is_empty() {
            return "local".to_string();
        }
        // Strip a leading ssh/mosh verb, then sanitize the target into a slug.
        let target = conn
            .strip_prefix("ssh ")
            .or_else(|| conn.strip_prefix("mosh "))
            .unwrap_or(conn)
            .trim();
        format!("term-{}", target.replace([' ', '@', ':', '/'], "-"))
    }

    fn sandbox_key(&self) -> &str {
        self.sandbox_rows
            .get(self.sandbox_sel)
            .map(|(k, _)| k.as_str())
            .unwrap_or("auto")
    }

    fn focus_down(&mut self) {
        self.focus = match self.focus {
            Field::Name => Field::Connection,
            // Skip Sandbox for a remote terminal — it's host-managed there.
            Field::Connection if self.is_local() => Field::Sandbox,
            Field::Connection => Field::Connection,
            Field::Sandbox => Field::Sandbox,
        };
    }

    fn focus_up(&mut self) {
        self.focus = match self.focus {
            Field::Sandbox => Field::Connection,
            Field::Connection => Field::Name,
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
            Field::Connection | Field::Sandbox => self.connection.push_str(&clean),
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
            // Connection is a free-text field; Enter submits from here when the
            // terminal is remote (no Sandbox step), else advances to Sandbox.
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
        let local = self.is_local();
        Outcome::Submit(TerminalChoice {
            name: self.resolved_name(),
            kind: if local { "local" } else { "remote" }.to_string(),
            connection: self.connection.trim().to_string(),
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
        let spec = LayerSpec {
            title: "new terminal".to_string(),
            badge: Some(" Alt+T ".into()),
            cols: 54,
            rows: 3 + 2,
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

        // --- connection (editable) ----------------------------------------
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

        // --- sandbox (inline cycle when local; remote-managed otherwise) ---
        let sb_focused = self.focus == Field::Sandbox;
        if self.is_local() {
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
                    seg(Tok::Slot(S::Dim), "· managed by remote".to_string()),
                ]),
                panel,
            );
        }

        // Footer hint.
        let enter_verb = if self.focus == Field::Name {
            "enter next"
        } else {
            "enter create"
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
        TerminalWizard::new(&Config::default())
    }

    #[test]
    fn local_shell_derives_name_and_carries_sandbox() {
        let mut w = wiz();
        // Pick a concrete backend on the (skipped-to) Sandbox field.
        w.focus = Field::Sandbox;
        // Force a known backend regardless of the default ordering.
        w.sandbox_rows = vec![("bwrap".into(), "▣ Bubblewrap".into())];
        w.sandbox_sel = 0;
        match w.submit() {
            Outcome::Submit(c) => {
                assert_eq!(c.name, "local");
                assert_eq!(c.kind, "local");
                assert_eq!(c.connection, "");
                assert_eq!(c.sandbox, "bwrap");
            }
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn remote_connection_derives_name_and_drops_sandbox() {
        let mut w = wiz();
        for c in "ssh user@host".chars() {
            w.connection.push(c);
        }
        match w.submit() {
            Outcome::Submit(c) => {
                assert_eq!(c.kind, "remote");
                assert_eq!(c.connection, "ssh user@host");
                assert_eq!(c.name, "term-user-host");
                assert_eq!(c.sandbox, "", "remote terminals carry no host sandbox");
            }
            _ => panic!("expected submit"),
        }
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
