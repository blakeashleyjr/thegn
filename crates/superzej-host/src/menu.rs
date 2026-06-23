//! The lazygit-style option/confirm menu for the git panel: a small fixed
//! list with letter hotkeys, j/k + Enter selection, Esc cancel, painted as a
//! centered layer ([`LayerSpec`]/[`open_layer`] — dim backdrop, shadow, boxed
//! border). Not a fuzzy palette: every menu is built whole by a constructor
//! below and the loop dispatches on the typed [`MenuChoice`] it picks.
//!
//! j/k/q are reserved for navigation and cancel, so no constructor may hand
//! out those letters as hotkeys (asserted in tests, debug-asserted in
//! [`MenuOverlay::new`] alongside hotkey uniqueness).
#![allow(dead_code)] // the loop adopts menus incrementally; choices/constructors land ahead of their call sites

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use superzej_core::theme::Hue;

/// A typed menu outcome the event loop dispatches on.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuChoice {
    // rebase options (m)
    RebaseContinue,
    RebaseAbort,
    RebaseSkip,
    // reset menu (D) — sha of the selected commit (or HEAD)
    ResetSoft(String),
    ResetMixed(String),
    ResetHard(String),
    Nuke,
    // custom patch options (C-p)
    PatchApply,
    PatchApplyReverse,
    PatchToIndex,
    PatchNewCommit,
    PatchRemoveFromCommit,
    PatchReset,
    // diff-mode menu (W)
    DiffSwap,
    DiffExit,
    // bisect menu (b)
    BisectStart,
    BisectMarkGood,
    BisectMarkBad,
    BisectSkip,
    BisectReset,
    // branch actions
    BranchDelete { name: String, force: bool },
    BranchForcePush,
    BranchPush,
    BranchPull,
    BranchSetUpstream(String),
    BranchRename(String),
    BranchMerge(String),
    BranchCreate,
    // cherry-pick conflict options
    CherryContinue,
    CherryAbort,
    CherrySkip,
    // merge conflict options
    MergeContinue,
    MergeAbort,
    // revert conflict options
    RevertContinue,
    RevertAbort,
    // fetch all remotes
    BranchFetch,
    // custom command by index into the config list
    CustomCommand(usize),
    // undo/redo confirmation (carries the human description shown)
    ConfirmUndo,
    ConfirmRedo,
    // generic yes/no confirm — the loop interprets `tag`
    Confirm { tag: &'static str, arg: String },
    // first-launch keymap picker (item 621): the chosen preset id
    // ("default" | "vscode" | "jetbrains").
    SetKeymapPreset(String),
    Dismiss,
}

/// Why the menu exists (the loop uses this to rebuild on state change).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKindTag {
    Rebase,
    Reset,
    Patch,
    Diff,
    BranchActions,
    CustomCommands,
    Bisect,
    Keybinds,
    UndoConfirm,
    RedoConfirm,
    Confirm,
    KeymapPicker,
}

/// One selectable row: an optional letter hotkey (rendered as a chip), the
/// label, a dim right-hand note, and the choice Enter/hotkey resolves to.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub key: Option<char>,
    pub label: String,
    pub note: Option<String>,
    pub choice: MenuChoice,
    /// Render the label in red; Enter still works.
    pub danger: bool,
}

fn item(key: Option<char>, label: impl Into<String>, choice: MenuChoice) -> MenuItem {
    MenuItem {
        key,
        label: label.into(),
        note: None,
        choice,
        danger: false,
    }
}

impl MenuItem {
    fn note(mut self, note: impl Into<String>) -> MenuItem {
        self.note = Some(note.into());
        self
    }
    fn danger(mut self) -> MenuItem {
        self.danger = true;
        self
    }
}

/// The open menu: a fixed item list with one selected row. The loop holds an
/// `Option<MenuOverlay>` the same way it holds the palette, feeds keys through
/// [`MenuOverlay::handle_key`], and paints it last via [`MenuOverlay::render`].
#[derive(Debug)]
pub struct MenuOverlay {
    pub tag: MenuKindTag,
    pub title: String,
    items: Vec<MenuItem>,
    selected: usize,
    /// A non-selectable note row above the items (the confirm body).
    body: Option<String>,
}

/// What a key delivered to the menu meant.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuOutcome {
    Pending,
    Cancel,
    Pick(MenuChoice),
}

/// Keys the menu itself owns; constructors must not hand them out as hotkeys.
const RESERVED: [char; 3] = ['j', 'k', 'q'];

impl MenuOverlay {
    pub fn new(tag: MenuKindTag, title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        #[cfg(debug_assertions)]
        {
            let keys: Vec<char> = items
                .iter()
                .filter_map(|i| i.key)
                .map(|c| c.to_ascii_lowercase())
                .collect();
            for (n, k) in keys.iter().enumerate() {
                debug_assert!(
                    !keys[..n].contains(k),
                    "duplicate menu hotkey {k:?} (matching is case-insensitive)"
                );
                debug_assert!(!RESERVED.contains(k), "menu hotkey {k:?} is reserved");
            }
        }
        MenuOverlay {
            tag,
            title: title.into(),
            items,
            selected: 0,
            body: None,
        }
    }

    fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// j/k/↑↓ move (clamped at the ends); Enter picks the selected; a letter
    /// hotkey picks its item immediately (case-insensitive); Esc/q cancels;
    /// everything else is Pending.
    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> MenuOutcome {
        if mods.contains(Modifiers::CTRL) {
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => MenuOutcome::Cancel,
                _ => MenuOutcome::Pending,
            };
        }
        if mods.intersects(Modifiers::ALT | Modifiers::SUPER) {
            return MenuOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return MenuOutcome::Cancel;
        }
        match key {
            KeyCode::Char('q') => MenuOutcome::Cancel,
            KeyCode::DownArrow | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
                MenuOutcome::Pending
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                MenuOutcome::Pending
            }
            KeyCode::Enter => match self.items.get(self.selected) {
                Some(it) => MenuOutcome::Pick(it.choice.clone()),
                None => MenuOutcome::Cancel,
            },
            KeyCode::Char(c) => {
                let want = c.to_ascii_lowercase();
                match self
                    .items
                    .iter()
                    .find(|i| i.key.map(|k| k.to_ascii_lowercase()) == Some(want))
                {
                    Some(it) => MenuOutcome::Pick(it.choice.clone()),
                    None => MenuOutcome::Pending,
                }
            }
            _ => MenuOutcome::Pending,
        }
    }

    /// Content width: the widest `❯ [k] label … note` row (or the body),
    /// before open_layer's clamp-to-screen.
    fn content_cols(&self) -> usize {
        let row = |it: &MenuItem| {
            2 + it.key.map_or(0, |_| 4)
                + it.label.chars().count()
                + it.note.as_ref().map_or(0, |n| 2 + n.chars().count())
        };
        self.items
            .iter()
            .map(row)
            .chain(self.body.iter().map(|b| b.chars().count()))
            .chain(self.title.chars().count().checked_add(6))
            .max()
            .unwrap_or(0)
            .max(24)
    }

    /// Paint as a centered layer: one row per item — `[key] label … note` —
    /// the selected row accent-tinted behind a `❯` marker, danger labels red.
    /// Sized to content (open_layer clamps to the screen) like the palette.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let body_rows = usize::from(self.body.is_some());
        let spec = LayerSpec {
            title: self.title.clone(),
            badge: Some(" menu ".into()),
            cols: self.content_cols(),
            rows: body_rows + self.items.len(),
            anchor: Anchor::Center,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);

        if let Some(body) = &self.body {
            seg::draw_line(
                surface,
                inner.x,
                inner.y,
                inner.cols,
                &Line::segs(vec![seg(Tok::Slot(S::Dim), body.clone())]),
                panel,
            );
        }

        for (row, it) in self.items.iter().take(inner.rows - body_rows).enumerate() {
            let selected = row == self.selected;
            let pad = if selected { Tok::SelAccent } else { panel };
            let mut left: Vec<Seg> = vec![if selected {
                seg(Tok::Slot(S::Accent), "❯ ").bold()
            } else {
                sp(2)
            }];
            if let Some(k) = it.key {
                left.push(Seg::chip(Tok::Slot(S::Raise), format!(" {k} ")));
                left.push(sp(1));
            }
            let label_fg = if it.danger {
                Tok::Hue(Hue::Red)
            } else if selected {
                Tok::Slot(S::Text)
            } else {
                Tok::Slot(S::Dim)
            };
            let mut label = seg(label_fg, it.label.clone());
            if selected {
                label = label.bold();
            }
            left.push(label);
            let right: Vec<Seg> = it
                .note
                .iter()
                .map(|n| seg(Tok::Slot(S::Ghost), n.clone()))
                .collect();
            seg::draw_line(
                surface,
                inner.x,
                inner.y + body_rows + row,
                inner.cols,
                &Line::split(left, right),
                pad,
            );
        }
    }
}

/// A 2-item yes/no confirm built on the same component: `[y]` resolves to
/// `Confirm { tag, arg }`, `[n]` to `Dismiss`; `body` is the non-selectable
/// note row above them.
pub fn confirm_menu(
    title: impl Into<String>,
    body: impl Into<String>,
    tag: &'static str,
    arg: String,
    danger: bool,
) -> MenuOverlay {
    let mut yes = item(Some('y'), "confirm", MenuChoice::Confirm { tag, arg });
    if danger {
        yes = yes.danger();
    }
    MenuOverlay::new(
        MenuKindTag::Confirm,
        title,
        vec![yes, item(Some('n'), "cancel", MenuChoice::Dismiss)],
    )
    .with_body(body)
}

/// First-launch keymap picker (item 621): pick a familiar IDE keymap overlay or
/// keep superzej's defaults. Each choice resolves to `SetKeymapPreset`.
pub fn keymap_preset_menu() -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::KeymapPicker,
        "Choose a keymap",
        vec![
            item(
                Some('d'),
                "superzej defaults",
                MenuChoice::SetKeymapPreset("default".into()),
            )
            .note("Alt/Ctrl chords"),
            item(
                Some('v'),
                "VS Code",
                MenuChoice::SetKeymapPreset("vscode".into()),
            )
            .note("Ctrl+P, Ctrl+Shift+P, Ctrl+B"),
            item(
                Some('i'),
                "JetBrains",
                MenuChoice::SetKeymapPreset("jetbrains".into()),
            )
            .note("Ctrl+Shift+A, Ctrl+E"),
        ],
    )
    .with_body("Familiar shortcuts on top of the defaults. Change later with keymap_preset.")
}

/// Rebase options: continue + skip only while a rebase is conflicted/running.
pub fn rebase_menu(conflicted: bool) -> MenuOverlay {
    let mut items = Vec::new();
    if conflicted {
        items.push(item(
            Some('c'),
            "continue rebase",
            MenuChoice::RebaseContinue,
        ));
        items.push(item(Some('s'), "skip commit", MenuChoice::RebaseSkip));
    }
    items.push(item(Some('a'), "abort rebase", MenuChoice::RebaseAbort).danger());
    MenuOverlay::new(MenuKindTag::Rebase, "rebase", items)
}

/// Reset to a commit: soft/mixed/hard against `sha` (labelled with `short`)
/// plus the nuke-working-tree escape hatch; hard + nuke are danger rows.
pub fn reset_menu(sha: &str, short: &str) -> MenuOverlay {
    let to = format!("to {short}");
    MenuOverlay::new(
        MenuKindTag::Reset,
        "reset",
        vec![
            item(Some('s'), "soft reset", MenuChoice::ResetSoft(sha.into())).note(&to),
            item(Some('m'), "mixed reset", MenuChoice::ResetMixed(sha.into())).note(&to),
            item(Some('h'), "hard reset", MenuChoice::ResetHard(sha.into()))
                .note(&to)
                .danger(),
            item(Some('n'), "nuke working tree", MenuChoice::Nuke)
                .note("reset --hard + clean -fd")
                .danger(),
        ],
    )
}

/// Custom-patch options for the building-patch mode.
pub fn patch_menu() -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::Patch,
        "custom patch",
        vec![
            item(Some('a'), "apply patch", MenuChoice::PatchApply),
            item(
                Some('r'),
                "apply patch in reverse",
                MenuChoice::PatchApplyReverse,
            ),
            item(
                Some('i'),
                "move patch out into index",
                MenuChoice::PatchToIndex,
            ),
            item(
                Some('c'),
                "move patch into new commit",
                MenuChoice::PatchNewCommit,
            ),
            item(
                Some('d'),
                "remove patch from original commit",
                MenuChoice::PatchRemoveFromCommit,
            )
            .danger(),
            item(Some('x'), "reset patch", MenuChoice::PatchReset),
        ],
    )
}

/// Diff-mode options while comparing against `marked`.
pub fn diff_menu(marked: &str) -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::Diff,
        "diffing",
        vec![
            item(Some('s'), "swap diff sides", MenuChoice::DiffSwap).note(format!("vs {marked}")),
            item(Some('x'), "exit diff mode", MenuChoice::DiffExit),
        ],
    )
}

/// Bisect options: start when inactive; mark good/bad/skip/reset while active.
pub fn bisect_menu(active: bool) -> MenuOverlay {
    let items = if active {
        vec![
            item(Some('g'), "mark commit good", MenuChoice::BisectMarkGood),
            item(Some('b'), "mark commit bad", MenuChoice::BisectMarkBad),
            item(Some('s'), "skip commit", MenuChoice::BisectSkip),
            item(Some('r'), "reset bisect", MenuChoice::BisectReset),
        ]
    } else {
        vec![item(Some('s'), "start bisect", MenuChoice::BisectStart)]
    };
    MenuOverlay::new(MenuKindTag::Bisect, "bisect", items)
}

/// Actions on branch `name`; delete is omitted for the checked-out branch.
pub fn branch_menu(name: &str, is_head: bool) -> MenuOverlay {
    let mut items = Vec::new();
    if !is_head {
        items.push(
            item(
                Some('d'),
                format!("delete {name}"),
                MenuChoice::BranchDelete {
                    name: name.into(),
                    force: false,
                },
            )
            .note("forces if unmerged")
            .danger(),
        );
    }
    items.push(item(Some('f'), "force push", MenuChoice::BranchForcePush).danger());
    items.push(item(Some('p'), "push", MenuChoice::BranchPush));
    items.push(item(Some('l'), "pull", MenuChoice::BranchPull));
    items.push(item(
        Some('u'),
        "set upstream",
        MenuChoice::BranchSetUpstream(name.into()),
    ));
    items.push(item(
        Some('r'),
        "rename",
        MenuChoice::BranchRename(name.into()),
    ));
    MenuOverlay::new(MenuKindTag::BranchActions, name, items)
}

/// Merge-conflict options.
pub fn merge_conflict_menu() -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::Confirm,
        "merge conflicts",
        vec![
            item(Some('c'), "continue merge", MenuChoice::MergeContinue),
            item(Some('a'), "abort merge", MenuChoice::MergeAbort).danger(),
        ],
    )
}

/// Cherry-pick-conflict options.
pub fn cherry_conflict_menu() -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::Confirm,
        "cherry-pick conflicts",
        vec![
            item(
                Some('c'),
                "continue cherry-pick",
                MenuChoice::CherryContinue,
            ),
            item(Some('s'), "skip commit", MenuChoice::CherrySkip),
            item(Some('a'), "abort cherry-pick", MenuChoice::CherryAbort).danger(),
        ],
    )
}

/// Continue/abort while a revert is stopped on conflicts.
pub fn revert_conflict_menu() -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::Confirm,
        "revert conflicts",
        vec![
            item(Some('c'), "continue revert", MenuChoice::RevertContinue),
            item(Some('a'), "abort revert", MenuChoice::RevertAbort).danger(),
        ],
    )
}

/// The user's `[[commands]]` list from config: each row picks
/// `CustomCommand(i)` for its index.
pub fn custom_commands_menu(cmds: &[(char, String)]) -> MenuOverlay {
    let items = cmds
        .iter()
        .enumerate()
        .map(|(i, (key, label))| item(Some(*key), label.clone(), MenuChoice::CustomCommand(i)))
        .collect();
    MenuOverlay::new(MenuKindTag::CustomCommands, "custom commands", items)
}

/// A yes/no confirm for a computed undo/redo plan: `[y]` resolves to
/// `ConfirmUndo`/`ConfirmRedo` (the loop holds the plan), `[n]` dismisses.
pub fn undo_confirm_menu(body: impl Into<String>, redo: bool) -> MenuOverlay {
    let (tag, title, choice) = if redo {
        (MenuKindTag::RedoConfirm, "redo?", MenuChoice::ConfirmRedo)
    } else {
        (MenuKindTag::UndoConfirm, "undo?", MenuChoice::ConfirmUndo)
    };
    MenuOverlay::new(
        tag,
        title,
        vec![
            item(Some('y'), "confirm", choice).danger(),
            item(Some('n'), "cancel", MenuChoice::Dismiss),
        ],
    )
    .with_body(body)
}

/// Branch actions including create + merge (the full `m`/`n` menu); merge
/// and delete are omitted for the checked-out branch.
pub fn branch_actions_menu(name: &str, is_head: bool) -> MenuOverlay {
    let mut m = branch_menu(name, is_head);
    m.items
        .push(item(Some('n'), "new branch", MenuChoice::BranchCreate));
    m.items.push(item(
        Some('g'),
        "fetch all remotes (--prune)",
        MenuChoice::BranchFetch,
    ));
    if !is_head {
        m.items.push(item(
            Some('m'),
            format!("merge {name} into checked-out"),
            MenuChoice::BranchMerge(name.into()),
        ));
    }
    m
}

/// A read-only output popup (captured custom-command output): one row per
/// line, every pick dismisses. Long outputs truncate with a count row.
pub fn output_menu(title: impl Into<String>, text: &str) -> MenuOverlay {
    const CAP: usize = 30;
    let lines: Vec<&str> = text.lines().collect();
    let mut items: Vec<MenuItem> = lines
        .iter()
        .take(CAP)
        .map(|l| item(None, l.to_string(), MenuChoice::Dismiss))
        .collect();
    if lines.len() > CAP {
        items.push(item(
            None,
            format!("… +{} more lines", lines.len() - CAP),
            MenuChoice::Dismiss,
        ));
    }
    if items.is_empty() {
        items.push(item(None, "(no output)", MenuChoice::Dismiss));
    }
    MenuOverlay::new(MenuKindTag::CustomCommands, title, items)
}

/// Commit-input toggles (item 328) carried by the commit message overlay:
/// Ctrl+N flips hook-skipping, Ctrl+S cycles signing. Rendered as a badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitToggles {
    /// `--no-verify`: skip pre-commit / commit-msg hooks.
    pub no_verify: bool,
    /// Signing override: `None` inherit · `Some(true)` `--gpg-sign` ·
    /// `Some(false)` `--no-gpg-sign`.
    pub sign: Option<bool>,
}

impl CommitToggles {
    /// Cycle the signing override: inherit → sign → no-sign → inherit.
    fn cycle_sign(&mut self) {
        self.sign = match self.sign {
            None => Some(true),
            Some(true) => Some(false),
            Some(false) => None,
        };
    }

    /// Compact badge text reflecting the current toggles.
    fn badge(self) -> String {
        let sign = match self.sign {
            None => "sign:auto",
            Some(true) => "sign:on",
            Some(false) => "sign:off",
        };
        let verify = if self.no_verify {
            "hooks:skip"
        } else {
            "hooks:on"
        };
        format!(" {sign} · {verify} ")
    }
}

/// A one-line text input overlay (commit message, stash message, command
/// prompts): printable keys edit, Backspace deletes, Enter submits, Esc (or
/// Ctrl+c) cancels. The loop holds it like the palette — feeds keys through
/// [`InputOverlay::handle_key`], paints it last via [`InputOverlay::render`] —
/// and pairs it with a purpose tag of its own.
#[derive(Debug)]
pub struct InputOverlay {
    pub title: String,
    value: String,
    /// When set, this is the commit-message overlay: Ctrl+S/Ctrl+N drive the
    /// signing / hook toggles (item 328) and the state shows as a badge.
    commit_toggles: Option<CommitToggles>,
}

/// What a key delivered to the input overlay meant.
#[derive(Debug, Clone, PartialEq)]
pub enum InputOutcome {
    Pending,
    Cancel,
    Submit(String),
}

impl InputOverlay {
    pub fn new(title: impl Into<String>, prefill: impl Into<String>) -> Self {
        InputOverlay {
            title: title.into(),
            value: prefill.into(),
            commit_toggles: None,
        }
    }

    /// The commit-message overlay: like [`new`], but Ctrl+S/Ctrl+N drive the
    /// signing / hook toggles (item 328).
    ///
    /// [`new`]: InputOverlay::new
    pub fn new_commit(title: impl Into<String>, prefill: impl Into<String>) -> Self {
        InputOverlay {
            title: title.into(),
            value: prefill.into(),
            commit_toggles: Some(CommitToggles::default()),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// The current commit toggles, if this is the commit overlay.
    pub fn commit_toggles(&self) -> Option<CommitToggles> {
        self.commit_toggles
    }

    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> InputOutcome {
        if mods.contains(Modifiers::CTRL) {
            // Commit-overlay toggles (item 328) — consumed, overlay stays open.
            if let Some(t) = &mut self.commit_toggles {
                match key {
                    KeyCode::Char('n' | 'N') => {
                        t.no_verify = !t.no_verify;
                        return InputOutcome::Pending;
                    }
                    KeyCode::Char('s' | 'S') => {
                        t.cycle_sign();
                        return InputOutcome::Pending;
                    }
                    _ => {}
                }
            }
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => InputOutcome::Cancel,
                _ => InputOutcome::Pending,
            };
        }
        if mods.contains(Modifiers::ALT) || mods.contains(Modifiers::SUPER) {
            return InputOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return InputOutcome::Cancel;
        }
        match key {
            KeyCode::Enter => InputOutcome::Submit(self.value.clone()),
            KeyCode::Backspace => {
                self.value.pop();
                InputOutcome::Pending
            }
            KeyCode::Char(c) => {
                self.value.push(*c);
                InputOutcome::Pending
            }
            _ => InputOutcome::Pending,
        }
    }

    /// Paint as a one-row centered layer: `❯ value▏`. The commit overlay shows
    /// its signing/hook toggles in the badge (item 328).
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let badge = match self.commit_toggles {
            Some(t) => t.badge(),
            None => " input ".into(),
        };
        let spec = LayerSpec {
            title: self.title.clone(),
            badge: Some(badge),
            cols: (self.value.chars().count() + 8)
                .max(self.title.chars().count() + 6)
                .max(40),
            rows: 1,
            anchor: Anchor::Center,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(vec![
                seg(Tok::Slot(S::Accent), "❯ ").bold(),
                seg(Tok::Slot(S::Text), self.value.clone()),
                seg(Tok::Slot(S::Accent), "▏"),
            ]),
            Tok::Slot(S::Panel),
        );
    }
}

/// A read-only cheatsheet of `(chord, description)` rows for the focused
/// view; every row resolves to `Dismiss`, so any pick just closes it.
pub fn keybinds_menu(view_label: &str, keys: &[(String, String)]) -> MenuOverlay {
    let items = keys
        .iter()
        .map(|(chord, desc)| item(None, desc.clone(), MenuChoice::Dismiss).note(chord.clone()))
        .collect();
    MenuOverlay::new(
        MenuKindTag::Keybinds,
        format!("keybinds — {view_label}"),
        items,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    fn three() -> MenuOverlay {
        MenuOverlay::new(
            MenuKindTag::Confirm,
            "t",
            vec![
                item(Some('a'), "alpha", MenuChoice::Dismiss),
                item(Some('b'), "beta", MenuChoice::ConfirmUndo),
                item(Some('c'), "gamma", MenuChoice::ConfirmRedo),
            ],
        )
    }

    fn hotkeys(m: &MenuOverlay) -> Vec<char> {
        m.items()
            .iter()
            .filter_map(|i| i.key)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }

    #[test]
    fn commit_overlay_toggles_cycle_signing_and_no_verify() {
        // Item 328: the commit overlay carries signing/hook toggles driven by
        // Ctrl+S / Ctrl+N; a plain input overlay has none.
        let mut ov = InputOverlay::new_commit("commit message", "");
        assert_eq!(ov.commit_toggles(), Some(CommitToggles::default()));

        // Ctrl+N toggles hooks; overlay stays open and the text is untouched.
        assert_eq!(
            ov.handle_key(&KeyCode::Char('n'), Modifiers::CTRL),
            InputOutcome::Pending
        );
        assert!(ov.commit_toggles().unwrap().no_verify);
        assert_eq!(ov.value(), "", "toggle key is not typed into the message");

        // Ctrl+S cycles signing: auto → on → off → auto.
        ov.handle_key(&KeyCode::Char('s'), Modifiers::CTRL);
        assert_eq!(ov.commit_toggles().unwrap().sign, Some(true));
        ov.handle_key(&KeyCode::Char('s'), Modifiers::CTRL);
        assert_eq!(ov.commit_toggles().unwrap().sign, Some(false));
        ov.handle_key(&KeyCode::Char('s'), Modifiers::CTRL);
        assert_eq!(ov.commit_toggles().unwrap().sign, None);

        // Ctrl+C still cancels.
        assert_eq!(
            ov.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            InputOutcome::Cancel
        );

        // A plain overlay has no toggles and types normal chars.
        let mut plain = InputOverlay::new("x", "");
        assert!(plain.commit_toggles().is_none());
        plain.handle_key(&KeyCode::Char('a'), NONE);
        assert_eq!(plain.value(), "a");

        // Badge reflects state.
        assert!(
            CommitToggles {
                no_verify: true,
                sign: Some(false)
            }
            .badge()
            .contains("hooks:skip")
        );
        assert!(
            CommitToggles {
                no_verify: false,
                sign: Some(true)
            }
            .badge()
            .contains("sign:on")
        );
    }

    #[test]
    fn jk_and_arrows_move_and_clamp() {
        let mut m = three();
        assert_eq!(m.selected(), 0);
        assert_eq!(
            m.handle_key(&KeyCode::Char('k'), NONE),
            MenuOutcome::Pending
        );
        assert_eq!(m.selected(), 0, "k clamps at the top");
        m.handle_key(&KeyCode::Char('j'), NONE);
        m.handle_key(&KeyCode::DownArrow, NONE);
        assert_eq!(m.selected(), 2);
        assert_eq!(
            m.handle_key(&KeyCode::Char('j'), NONE),
            MenuOutcome::Pending
        );
        assert_eq!(m.selected(), 2, "j clamps at the bottom");
        m.handle_key(&KeyCode::UpArrow, NONE);
        assert_eq!(m.selected(), 1);
    }

    #[test]
    fn enter_picks_the_selected_item() {
        let mut m = three();
        m.handle_key(&KeyCode::Char('j'), NONE);
        assert_eq!(
            m.handle_key(&KeyCode::Enter, NONE),
            MenuOutcome::Pick(MenuChoice::ConfirmUndo)
        );
    }

    #[test]
    fn hotkey_picks_directly_and_case_insensitively() {
        let mut m = three();
        assert_eq!(
            m.handle_key(&KeyCode::Char('c'), NONE),
            MenuOutcome::Pick(MenuChoice::ConfirmRedo)
        );
        assert_eq!(
            m.handle_key(&KeyCode::Char('B'), NONE),
            MenuOutcome::Pick(MenuChoice::ConfirmUndo)
        );
    }

    #[test]
    fn esc_and_q_cancel() {
        let mut m = three();
        assert_eq!(m.handle_key(&KeyCode::Escape, NONE), MenuOutcome::Cancel);
        assert_eq!(
            m.handle_key(&KeyCode::Char('\x1b'), NONE),
            MenuOutcome::Cancel,
            "CSI-u/fixterms Esc decodes as a literal ESC char"
        );
        assert_eq!(m.handle_key(&KeyCode::Char('q'), NONE), MenuOutcome::Cancel);
    }

    #[test]
    fn ctrl_c_and_ctrl_g_cancel_menu() {
        let mut m = three();
        assert_eq!(
            m.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            MenuOutcome::Cancel
        );
        assert_eq!(
            m.handle_key(&KeyCode::Char('G'), Modifiers::CTRL),
            MenuOutcome::Cancel
        );
    }

    #[test]
    fn unknown_keys_and_modified_keys_are_pending() {
        let mut m = three();
        assert_eq!(
            m.handle_key(&KeyCode::Char('z'), NONE),
            MenuOutcome::Pending
        );
        assert_eq!(m.handle_key(&KeyCode::Tab, NONE), MenuOutcome::Pending);
        assert_eq!(
            m.handle_key(&KeyCode::Char('a'), Modifiers::CTRL),
            MenuOutcome::Pending,
            "ctrl-chords pass through"
        );
    }

    #[test]
    fn enter_on_an_empty_menu_cancels() {
        let mut m = MenuOverlay::new(MenuKindTag::Confirm, "t", Vec::new());
        assert_eq!(m.handle_key(&KeyCode::Enter, NONE), MenuOutcome::Cancel);
        assert_eq!(
            m.handle_key(&KeyCode::Char('j'), NONE),
            MenuOutcome::Pending
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "duplicate menu hotkey")]
    fn duplicate_hotkeys_debug_assert() {
        let _ = MenuOverlay::new(
            MenuKindTag::Confirm,
            "t",
            vec![
                item(Some('a'), "one", MenuChoice::Dismiss),
                item(Some('A'), "two", MenuChoice::Dismiss),
            ],
        );
    }

    #[test]
    fn every_constructor_has_unique_unreserved_hotkeys() {
        let menus = vec![
            rebase_menu(true),
            rebase_menu(false),
            reset_menu("deadbeef", "deadbee"),
            patch_menu(),
            diff_menu("main"),
            bisect_menu(true),
            bisect_menu(false),
            branch_menu("feature", false),
            branch_menu("main", true),
            branch_actions_menu("feature", false),
            branch_actions_menu("main", true),
            undo_confirm_menu("undo: commit?", false),
            undo_confirm_menu("redo: commit?", true),
            output_menu("output", "line one\nline two"),
            merge_conflict_menu(),
            cherry_conflict_menu(),
            custom_commands_menu(&[('t', "run tests".into()), ('z', "fmt".into())]),
            keybinds_menu("commits", &[("C-p".into(), "build patch".into())]),
            confirm_menu(
                "discard?",
                "discards file.rs",
                "discard",
                "file.rs".into(),
                true,
            ),
        ];
        for m in &menus {
            assert!(!m.items().is_empty(), "{:?} menu is empty", m.tag);
            let keys = hotkeys(m);
            let mut dedup = keys.clone();
            dedup.sort_unstable();
            dedup.dedup();
            assert_eq!(keys.len(), dedup.len(), "{:?} has duplicate hotkeys", m.tag);
            for k in &keys {
                assert!(
                    !RESERVED.contains(k),
                    "{:?} hands out reserved {k:?}",
                    m.tag
                );
            }
        }
    }

    #[test]
    fn rebase_menu_gates_continue_and_skip_on_conflict() {
        assert_eq!(hotkeys(&rebase_menu(true)), vec!['c', 's', 'a']);
        assert_eq!(hotkeys(&rebase_menu(false)), vec!['a']);
    }

    #[test]
    fn reset_menu_carries_sha_and_marks_danger() {
        let m = reset_menu("deadbeef", "deadbee");
        assert_eq!(hotkeys(&m), vec!['s', 'm', 'h', 'n']);
        assert_eq!(
            m.items()[0].choice,
            MenuChoice::ResetSoft("deadbeef".into())
        );
        assert_eq!(
            m.items()[1].choice,
            MenuChoice::ResetMixed("deadbeef".into())
        );
        assert_eq!(
            m.items()[2].choice,
            MenuChoice::ResetHard("deadbeef".into())
        );
        assert_eq!(m.items()[3].choice, MenuChoice::Nuke);
        let danger: Vec<bool> = m.items().iter().map(|i| i.danger).collect();
        assert_eq!(danger, vec![false, false, true, true]);
        assert_eq!(m.items()[0].note.as_deref(), Some("to deadbee"));
    }

    #[test]
    fn patch_and_diff_menus_have_expected_hotkeys() {
        assert_eq!(hotkeys(&patch_menu()), vec!['a', 'r', 'i', 'c', 'd', 'x']);
        let d = diff_menu("main");
        assert_eq!(hotkeys(&d), vec!['s', 'x']);
        assert_eq!(d.items()[0].note.as_deref(), Some("vs main"));
    }

    #[test]
    fn bisect_menu_swaps_start_for_marks_when_active() {
        assert_eq!(hotkeys(&bisect_menu(false)), vec!['s']);
        assert_eq!(hotkeys(&bisect_menu(true)), vec!['g', 'b', 's', 'r']);
        assert_eq!(
            bisect_menu(false).items()[0].choice,
            MenuChoice::BisectStart
        );
    }

    #[test]
    fn branch_menu_skips_delete_on_head_and_flags_danger() {
        let m = branch_menu("feature", false);
        assert_eq!(hotkeys(&m), vec!['d', 'f', 'p', 'l', 'u', 'r']);
        assert!(m.items()[0].danger, "delete is danger");
        assert!(m.items()[1].danger, "force push is danger");
        assert_eq!(
            m.items()[0].choice,
            MenuChoice::BranchDelete {
                name: "feature".into(),
                force: false,
            }
        );
        assert_eq!(
            m.items()[4].choice,
            MenuChoice::BranchSetUpstream("feature".into())
        );
        let head = branch_menu("main", true);
        assert_eq!(hotkeys(&head), vec!['f', 'p', 'l', 'u', 'r']);
    }

    #[test]
    fn conflict_menus_continue_and_abort() {
        let m = merge_conflict_menu();
        assert_eq!(hotkeys(&m), vec!['c', 'a']);
        assert_eq!(m.items()[1].choice, MenuChoice::MergeAbort);
        let c = cherry_conflict_menu();
        assert_eq!(hotkeys(&c), vec!['c', 's', 'a']);
        assert_eq!(c.items()[0].choice, MenuChoice::CherryContinue);
    }

    #[test]
    fn custom_commands_menu_indexes_the_config_list() {
        let m = custom_commands_menu(&[('t', "run tests".into()), ('z', "fmt".into())]);
        assert_eq!(hotkeys(&m), vec!['t', 'z']);
        assert_eq!(m.items()[0].choice, MenuChoice::CustomCommand(0));
        assert_eq!(m.items()[1].choice, MenuChoice::CustomCommand(1));
        assert_eq!(m.items()[1].label, "fmt");
    }

    #[test]
    fn keybinds_menu_is_a_dismiss_only_cheatsheet() {
        let m = keybinds_menu(
            "commits",
            &[
                ("C-p".into(), "build patch".into()),
                ("W".into(), "diff mode".into()),
            ],
        );
        assert_eq!(m.tag, MenuKindTag::Keybinds);
        assert!(m.title.contains("commits"));
        assert!(!m.items().is_empty());
        for it in m.items() {
            assert_eq!(it.choice, MenuChoice::Dismiss);
            assert!(it.key.is_none());
            assert!(it.note.is_some(), "chord shown as the note");
        }
    }

    #[test]
    fn branch_actions_menu_extends_branch_menu_with_create_and_merge() {
        let m = branch_actions_menu("feature", false);
        assert_eq!(
            hotkeys(&m),
            vec!['d', 'f', 'p', 'l', 'u', 'r', 'n', 'g', 'm']
        );
        assert_eq!(
            m.items().last().unwrap().choice,
            MenuChoice::BranchMerge("feature".into())
        );
        let head = branch_actions_menu("main", true);
        assert_eq!(hotkeys(&head), vec!['f', 'p', 'l', 'u', 'r', 'n', 'g']);
        assert_eq!(head.items().last().unwrap().choice, MenuChoice::BranchFetch);
    }

    #[test]
    fn undo_confirm_menu_carries_the_plan_polarity() {
        let mut u = undo_confirm_menu("undo: commit?", false);
        assert_eq!(u.tag, MenuKindTag::UndoConfirm);
        assert_eq!(
            u.handle_key(&KeyCode::Char('y'), NONE),
            MenuOutcome::Pick(MenuChoice::ConfirmUndo)
        );
        let mut r = undo_confirm_menu("redo?", true);
        assert_eq!(r.tag, MenuKindTag::RedoConfirm);
        assert_eq!(
            r.handle_key(&KeyCode::Enter, NONE),
            MenuOutcome::Pick(MenuChoice::ConfirmRedo)
        );
    }

    #[test]
    fn output_menu_is_dismiss_only_and_truncates() {
        let m = output_menu("output", "a\nb");
        assert_eq!(m.items().len(), 2);
        assert!(m.items().iter().all(|i| i.choice == MenuChoice::Dismiss));
        let long: String = (0..50).map(|i| format!("l{i}\n")).collect();
        let m = output_menu("output", &long);
        assert_eq!(m.items().len(), 31);
        assert!(m.items().last().unwrap().label.contains("+20 more"));
        let empty = output_menu("output", "");
        assert_eq!(empty.items().len(), 1);
        assert!(empty.items()[0].label.contains("no output"));
    }

    #[test]
    fn input_overlay_edits_submits_and_cancels() {
        let mut i = InputOverlay::new("commit message", "fix");
        assert_eq!(i.value(), "fix");
        assert_eq!(
            i.handle_key(&KeyCode::Char('!'), NONE),
            InputOutcome::Pending
        );
        assert_eq!(i.value(), "fix!");
        i.handle_key(&KeyCode::Backspace, NONE);
        assert_eq!(i.value(), "fix");
        assert_eq!(
            i.handle_key(&KeyCode::Enter, NONE),
            InputOutcome::Submit("fix".into())
        );
        assert_eq!(i.handle_key(&KeyCode::Escape, NONE), InputOutcome::Cancel);
        assert_eq!(
            i.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            InputOutcome::Cancel
        );
        assert_eq!(
            i.handle_key(&KeyCode::Char('\x1b'), NONE),
            InputOutcome::Cancel,
            "CSI-u/fixterms Esc must cancel, not edit the text"
        );
        assert_eq!(i.value(), "fix");
        // Other ctrl-chords pass through without editing.
        assert_eq!(
            i.handle_key(&KeyCode::Char('x'), Modifiers::CTRL),
            InputOutcome::Pending
        );
        assert_eq!(i.value(), "fix");
    }

    #[test]
    fn input_overlay_renders_title_badge_and_value() {
        let i = InputOverlay::new("stash message", "wip stuff");
        let mut s = Surface::new(80, 24);
        i.render(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("stash message"), "{text:?}");
        assert!(text.contains("input"), "badge drawn");
        assert!(text.contains("wip stuff"), "value drawn");
        assert!(text.contains('❯'), "prompt marker drawn");
        // Tiny screens refuse without panicking.
        let mut tiny = Surface::new(6, 3);
        i.render(
            &mut tiny,
            Rect {
                x: 0,
                y: 0,
                cols: 6,
                rows: 3,
            },
        );
    }

    #[test]
    fn confirm_menu_yields_tagged_confirm_or_dismiss() {
        let mut m = confirm_menu(
            "discard?",
            "discards file.rs",
            "discard",
            "file.rs".into(),
            true,
        );
        assert_eq!(m.tag, MenuKindTag::Confirm);
        assert!(m.items()[0].danger, "danger yes row");
        assert_eq!(
            m.handle_key(&KeyCode::Char('y'), NONE),
            MenuOutcome::Pick(MenuChoice::Confirm {
                tag: "discard",
                arg: "file.rs".into(),
            })
        );
        assert_eq!(
            m.handle_key(&KeyCode::Char('n'), NONE),
            MenuOutcome::Pick(MenuChoice::Dismiss)
        );
    }

    #[test]
    fn keymap_preset_menu_offers_three_presets() {
        // Construction asserts hotkeys don't collide / hit reserved j/k/q.
        let mut m = keymap_preset_menu();
        assert_eq!(m.tag, MenuKindTag::KeymapPicker);
        assert_eq!(m.items().len(), 3);
        assert_eq!(
            m.handle_key(&KeyCode::Char('v'), NONE),
            MenuOutcome::Pick(MenuChoice::SetKeymapPreset("vscode".into()))
        );
        let mut m2 = keymap_preset_menu();
        assert_eq!(
            m2.handle_key(&KeyCode::Char('i'), NONE),
            MenuOutcome::Pick(MenuChoice::SetKeymapPreset("jetbrains".into()))
        );
        let mut m3 = keymap_preset_menu();
        assert_eq!(
            m3.handle_key(&KeyCode::Char('d'), NONE),
            MenuOutcome::Pick(MenuChoice::SetKeymapPreset("default".into()))
        );
    }

    #[test]
    fn render_draws_title_items_badge_and_marker_into_surface() {
        let mut m = reset_menu("deadbeef", "deadbee");
        m.handle_key(&KeyCode::Char('j'), NONE); // marker on row 1
        let mut s = Surface::new(80, 24);
        m.render(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("reset"), "layer title drawn: {text:?}");
        assert!(text.contains("menu"), "badge drawn");
        assert!(text.contains("soft reset"), "item label drawn");
        assert!(text.contains("nuke working tree"), "last item drawn");
        assert!(text.contains("to deadbee"), "note drawn");
        assert!(text.contains('❯'), "selected-row marker drawn");
        assert!(text.contains(" s "), "hotkey chip drawn");
    }

    #[test]
    fn render_includes_the_confirm_body_row() {
        let m = confirm_menu("discard?", "discards file.rs", "discard", "x".into(), false);
        let mut s = Surface::new(80, 24);
        m.render(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("discard?"), "title drawn");
        assert!(text.contains("discards file.rs"), "body row drawn");
        assert!(text.contains("confirm") && text.contains("cancel"));
    }

    #[test]
    fn render_survives_tiny_screens() {
        let m = patch_menu();
        let mut s = Surface::new(6, 3);
        m.render(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 6,
                rows: 3,
            },
        ); // open_layer refuses; must not panic
    }
}
