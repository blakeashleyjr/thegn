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
    DiffExit,
    // bisect menu (b)
    BisectStart,
    BisectMarkGood,
    BisectMarkBad,
    BisectSkip,
    BisectReset,
    // branch actions
    BranchDelete {
        name: String,
        force: bool,
    },
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
    Confirm {
        tag: &'static str,
        arg: String,
    },
    // delete worktree confirm: variant to capture "leave files" intent
    ConfirmDeleteWorktrees {
        keep_files: bool,
    },
    // delete workspace confirm: variant to capture "leave files" intent
    ConfirmDeleteWorkspace {
        keep_files: bool,
    },
    // init git confirm
    ConfirmInitGit {
        path: String,
    },
    // new-project confirm: mkdir the leaf (parent exists) + git init + open.
    ConfirmCreateProject {
        path: String,
    },
    // first-launch keymap picker (item 621): the chosen preset id
    // ("default" | "vscode" | "jetbrains").
    SetKeymapPreset(String),
    // bouncer tool-approval gate: allow/deny the sealed agent's pending shell /
    // edit / write tool call. The four ACP permission options — allow/reject,
    // each once or "always" (session-remembered). Esc/cancel = reject once.
    ApproveTool {
        decision: crate::bouncer::ApprovalDecision,
    },
    // share reach picker: the chosen reach (public/team/peer).
    ShareReach(superzej_core::config::ShareReach),
    // sandbox bring-up failed (failover off): retry the active worktree's env.
    SandboxRetry,
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
    Approval,
    ShareReach,
    SandboxHalt,
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
        Self::new_with_default(tag, title, items, 0)
    }

    pub fn new_with_default(
        tag: MenuKindTag,
        title: impl Into<String>,
        items: Vec<MenuItem>,
        selected: usize,
    ) -> Self {
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
            selected,
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
                left.push(Seg::key(format!(" {k} ")));
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

pub fn delete_worktree_menu(targets: usize, names_csv: &str) -> MenuOverlay {
    let title = format!("Delete {} worktree(s)?", targets);
    MenuOverlay::new_with_default(
        MenuKindTag::Confirm,
        title,
        vec![
            item(
                Some('y'),
                "delete from disk",
                MenuChoice::ConfirmDeleteWorktrees { keep_files: false },
            )
            .danger(),
            item(
                // 'k' is a reserved nav key (cursor-up); use 'f' (files) so the
                // hotkey is reachable and the debug-assert in `new_with_default`
                // stays happy.
                Some('f'),
                "keep files",
                MenuChoice::ConfirmDeleteWorktrees { keep_files: true },
            ),
            item(Some('n'), "cancel", MenuChoice::Dismiss),
        ],
        0,
    )
    .with_body(names_csv)
}

/// The dirty-tree variant of [`delete_worktree_menu`], shown whenever one or
/// more targets have uncommitted changes. It is raised EVEN when
/// `confirm_delete` is off — a destructive delete of unsaved work must never be
/// silent. Same three choices resolving to the same `ConfirmDeleteWorktrees` /
/// `Dismiss` as the clean menu (so the loop's Pick handler is reused untouched),
/// but the title/body shout that uncommitted work will be LOST and name the
/// dirty worktree(s), and the pre-selected default is the SAFE `cancel` (index
/// 2) rather than the destructive row — a dirty delete is never a single-Enter
/// mistake.
pub fn delete_worktree_menu_dirty(dirty: usize, dirty_csv: &str) -> MenuOverlay {
    let title = format!("Delete {dirty} worktree(s) with UNCOMMITTED changes?");
    MenuOverlay::new_with_default(
        MenuKindTag::Confirm,
        title,
        vec![
            item(
                Some('y'),
                "delete from disk (discard changes)",
                MenuChoice::ConfirmDeleteWorktrees { keep_files: false },
            )
            .danger(),
            item(
                // 'k' is a reserved nav key (cursor-up); use 'f' (files), matching
                // the clean menu and keeping new_with_default's debug-assert happy.
                Some('f'),
                "keep files (safe)",
                MenuChoice::ConfirmDeleteWorktrees { keep_files: true },
            ),
            item(Some('n'), "cancel", MenuChoice::Dismiss),
        ],
        2,
    )
    .with_body(format!("uncommitted work will be LOST in: {dirty_csv}"))
}

/// Confirm removing a workspace. Like `delete_worktree_menu`, the default
/// (pre-selected, index 0) choice is the destructive "delete from disk", which
/// removes every worktree directory of the workspace. `[f]` removes the
/// workspace from superzej only, leaving the files on disk.
pub fn delete_workspace_menu(display: &str) -> MenuOverlay {
    let title = format!("Delete workspace '{display}'?");
    MenuOverlay::new_with_default(
        MenuKindTag::Confirm,
        title,
        vec![
            item(
                Some('y'),
                "delete worktrees from disk",
                MenuChoice::ConfirmDeleteWorkspace { keep_files: false },
            )
            .danger(),
            item(
                // 'k' is a reserved nav key (cursor-up); use 'f' (files).
                Some('f'),
                "keep files on disk",
                MenuChoice::ConfirmDeleteWorkspace { keep_files: true },
            ),
            item(Some('n'), "cancel", MenuChoice::Dismiss),
        ],
        0,
    )
    .with_body("the home checkout is kept; branch worktrees are deleted")
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

/// The System ▸ Hosts action menu (opened with `m` on a host row): a
/// discoverable list mirroring the per-key accelerators, plus remove. Every
/// item resolves to a `Confirm { tag, arg: host_id }` routed by
/// [`crate::handlers::host::intercept_menu_choice`]. `remove host` is offered
/// only for runtime-added (DB) hosts — declarative `config.toml` hosts are
/// edited in the file.
pub fn host_actions_menu(name: &str, id: &str, is_db_def: bool) -> MenuOverlay {
    let arg = || id.to_string();
    let mut items = vec![
        item(
            Some('p'),
            "provision",
            MenuChoice::Confirm {
                tag: "host-provision",
                arg: arg(),
            },
        ),
        item(
            Some('r'),
            "re-probe",
            MenuChoice::Confirm {
                tag: "host-reprobe",
                arg: arg(),
            },
        ),
        item(
            Some('c'),
            "grant install consent",
            MenuChoice::Confirm {
                tag: "host-grant",
                arg: arg(),
            },
        ),
        item(
            Some('x'),
            "forget cached state",
            MenuChoice::Confirm {
                tag: "host-rm",
                arg: arg(),
            },
        )
        .danger(),
    ];
    if is_db_def {
        items.push(
            item(
                None,
                "remove host",
                MenuChoice::Confirm {
                    tag: "host-remove",
                    arg: arg(),
                },
            )
            .danger(),
        );
    }
    MenuOverlay::new(MenuKindTag::Confirm, format!("host: {name}"), items)
}

/// The bouncer's tool-approval gate: a sealed agent wants to `run`/`edit`/`write`
/// and the user picks one of ACP's four permission options. `[a]` allows once,
/// `[s]` allows for the rest of the session (remembered for this worktree +
/// action), `[d]` denies once, `[x]` denies for the session; `[d]`/Esc is the
/// safe default. `title` names the worktree + action (e.g. `pi · run a shell
/// command`); `body` is the command or path summary. Resolves to
/// `ApproveTool { decision }`.
pub fn approval_menu(title: impl Into<String>, body: impl Into<String>) -> MenuOverlay {
    use crate::bouncer::ApprovalDecision as D;
    MenuOverlay::new(
        MenuKindTag::Approval,
        title,
        vec![
            item(
                Some('a'),
                "allow once",
                MenuChoice::ApproveTool {
                    decision: D::AllowOnce,
                },
            ),
            item(
                Some('s'),
                "allow this session",
                MenuChoice::ApproveTool {
                    decision: D::AllowAlways,
                },
            )
            .note("remember for this action"),
            item(
                Some('d'),
                "deny once",
                MenuChoice::ApproveTool {
                    decision: D::RejectOnce,
                },
            )
            .danger(),
            item(
                Some('x'),
                "deny this session",
                MenuChoice::ApproveTool {
                    decision: D::RejectAlways,
                },
            )
            .danger(),
        ],
    )
    .with_body(body)
}

/// A non-local environment (provider/k8s/ssh) could not be brought up and
/// failover is off, so superzej refuses to silently open a host shell. `[r]`
/// retries the bring-up; `[n]`/Esc dismisses (the worktree stays without a pane).
/// `title` names the placement; `body` is the failure reason + how to allow
/// failover. Resolves to `SandboxRetry` or `Dismiss`.
pub fn sandbox_halt_menu(title: impl Into<String>, body: impl Into<String>) -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::SandboxHalt,
        title,
        vec![
            item(Some('r'), "retry bring-up", MenuChoice::SandboxRetry),
            item(Some('n'), "dismiss", MenuChoice::Dismiss),
        ],
    )
    .with_body(body)
}

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

/// Share reach picker (`Alt+Shift+S`): pick *who* the share is for; each reach
/// maps to a provider via `[share] public`/`team`/`peer`. Built from the
/// worktree's configured reaches; the public option is flagged as a caution.
pub fn reach_picker(cfg: &superzej_core::config::ShareConfig) -> MenuOverlay {
    use superzej_core::config::ShareReach;
    let items = cfg
        .configured_reaches()
        .into_iter()
        .map(|r| {
            let (glyph, key, desc) = match r {
                ShareReach::Public => ('\u{1f310}', 'p', "internet — anyone with the link"),
                ShareReach::Team => ('\u{1f465}', 't', "your tailnet / a teammate"),
                ShareReach::Peer => ('\u{1f517}', 'r', "a machine you hand a ticket to"),
            };
            let it = item(
                Some(key),
                format!("{glyph} {} — {desc}", r.as_str()),
                MenuChoice::ShareReach(r),
            )
            .note(cfg.reach_provider(r).as_str());
            if r == ShareReach::Public {
                it.danger()
            } else {
                it
            }
        })
        .collect();
    MenuOverlay::new(MenuKindTag::ShareReach, "Share to…", items)
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
        vec![item(Some('x'), "exit diff mode", MenuChoice::DiffExit).note(format!("vs {marked}"))],
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

pub fn init_git_menu(path: String) -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::Confirm,
        "initialize git repository?".to_string(),
        vec![
            item(
                Some('y'),
                "initialize git repo",
                MenuChoice::ConfirmInitGit { path: path.clone() },
            ),
            item(Some('n'), "cancel", MenuChoice::Dismiss),
        ],
    )
    .with_body(format!("{} is not a git repository", path))
}

/// Confirm creating a brand-new project dir (parent already exists) with a
/// fresh `git init`, then opening it as a workspace.
pub fn create_project_menu(path: String) -> MenuOverlay {
    MenuOverlay::new(
        MenuKindTag::Confirm,
        "create new project?".to_string(),
        vec![
            item(
                Some('y'),
                "create directory + git init",
                MenuChoice::ConfirmCreateProject { path: path.clone() },
            ),
            item(Some('n'), "cancel", MenuChoice::Dismiss),
        ],
    )
    .with_body(format!("{} does not exist yet", path))
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
    fn reach_picker_lists_configured_reaches_and_picks() {
        use superzej_core::config::{ShareConfig, ShareProviderKind, ShareReach};
        let cfg = ShareConfig {
            public: ShareProviderKind::Frp,
            team: ShareProviderKind::Tailscale,
            peer: ShareProviderKind::Iroh,
            ..ShareConfig::default()
        };
        let mut m = reach_picker(&cfg);
        // One item per configured reach, in public/team/peer order with p/t/r keys.
        assert_eq!(hotkeys(&m), vec!['p', 't', 'r']);
        // The public item carries its provider as the note and is danger-styled.
        let public = &m.items()[0];
        assert_eq!(public.note.as_deref(), Some("frp"));
        assert!(public.danger, "public reach should be a caution");
        // Hotkey 't' picks the team reach.
        assert_eq!(
            m.handle_key(&KeyCode::Char('t'), NONE),
            MenuOutcome::Pick(MenuChoice::ShareReach(ShareReach::Team))
        );
    }

    #[test]
    fn reach_picker_omits_public_when_disallowed() {
        use superzej_core::config::{ShareConfig, ShareProviderKind};
        let cfg = ShareConfig {
            public: ShareProviderKind::Frp,
            peer: ShareProviderKind::Iroh,
            allow_public: false,
            ..ShareConfig::default()
        };
        let m = reach_picker(&cfg);
        // public dropped (guard), only peer remains.
        assert_eq!(hotkeys(&m), vec!['r']);
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
            delete_worktree_menu(2, "a, b"),
            delete_worktree_menu_dirty(2, "a, b"),
            delete_workspace_menu("myrepo"),
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
    fn delete_workspace_menu_defaults_to_delete_from_disk() {
        let m = delete_workspace_menu("myrepo");
        // Default (pre-selected) item is the destructive "delete from disk".
        assert_eq!(m.selected(), 0);
        assert_eq!(
            m.items()[0].choice,
            MenuChoice::ConfirmDeleteWorkspace { keep_files: false }
        );
        assert!(m.items()[0].danger, "delete-from-disk row is danger-marked");
        assert_eq!(
            m.items()[1].choice,
            MenuChoice::ConfirmDeleteWorkspace { keep_files: true }
        );
        assert_eq!(m.items()[2].choice, MenuChoice::Dismiss);
        assert_eq!(hotkeys(&m), vec!['y', 'f', 'n']);
    }

    #[test]
    fn delete_worktree_menu_dirty_defaults_to_cancel_and_reuses_choices() {
        let mut m = delete_worktree_menu_dirty(2, "alpha, beta");
        // The SAFE option (cancel) is pre-selected on a dirty tree — a
        // destructive delete of unsaved work is never a single-Enter mistake.
        assert_eq!(m.selected(), 2);
        // Same hotkeys + same choice types as the clean menu, so the loop's
        // Pick handler is reused untouched.
        assert_eq!(hotkeys(&m), vec!['y', 'f', 'n']);
        assert_eq!(
            m.items()[0].choice,
            MenuChoice::ConfirmDeleteWorktrees { keep_files: false }
        );
        assert!(
            m.items()[0].danger,
            "delete-from-disk row stays danger-marked"
        );
        assert_eq!(
            m.items()[1].choice,
            MenuChoice::ConfirmDeleteWorktrees { keep_files: true }
        );
        assert!(!m.items()[1].danger, "keep-files row is not danger");
        assert_eq!(m.items()[2].choice, MenuChoice::Dismiss);
        // Hotkey 'y' picks the destructive delete; 'n' dismisses.
        assert_eq!(
            m.handle_key(&KeyCode::Char('y'), NONE),
            MenuOutcome::Pick(MenuChoice::ConfirmDeleteWorktrees { keep_files: false })
        );
        assert_eq!(
            delete_worktree_menu_dirty(1, "x").handle_key(&KeyCode::Char('n'), NONE),
            MenuOutcome::Pick(MenuChoice::Dismiss)
        );
        // The clean menu is unchanged: it still defaults to delete-from-disk.
        assert_eq!(delete_worktree_menu(1, "x").selected(), 0);
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
        assert_eq!(hotkeys(&d), vec!['x']);
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
    fn host_actions_menu_gates_remove_on_db_hosts() {
        // A config host offers p/r/c/x but not remove.
        let cfg_host = host_actions_menu("build-box", "host:build-box", false);
        let tags: Vec<&str> = cfg_host
            .items()
            .iter()
            .filter_map(|i| match &i.choice {
                MenuChoice::Confirm { tag, .. } => Some(*tag),
                _ => None,
            })
            .collect();
        assert_eq!(
            tags,
            vec!["host-provision", "host-reprobe", "host-grant", "host-rm"]
        );

        // A DB-added host adds the removal item, carrying the host id.
        let db_host = host_actions_menu("laptop", "host:laptop", true);
        let remove = db_host
            .items()
            .iter()
            .find(|i| {
                matches!(
                    &i.choice,
                    MenuChoice::Confirm {
                        tag: "host-remove",
                        ..
                    }
                )
            })
            .expect("db host offers remove");
        assert!(remove.danger, "remove is destructive");
        assert_eq!(
            remove.choice,
            MenuChoice::Confirm {
                tag: "host-remove",
                arg: "host:laptop".into(),
            }
        );
    }

    #[test]
    fn approval_menu_offers_four_acp_options() {
        use crate::bouncer::ApprovalDecision as D;
        let mut m = approval_menu("pi · run a shell command", "git status");
        assert_eq!(m.tag, MenuKindTag::Approval);
        // allow-once (0) and allow-session (1) are safe; the two deny rows are
        // danger-tinted.
        assert!(!m.items()[0].danger && !m.items()[1].danger);
        assert!(
            m.items()[2].danger && m.items()[3].danger,
            "deny rows danger"
        );
        // [a] allow once, [s] allow this session, [d] deny once, [x] deny session.
        assert_eq!(
            m.handle_key(&KeyCode::Char('a'), NONE),
            MenuOutcome::Pick(MenuChoice::ApproveTool {
                decision: D::AllowOnce
            })
        );
        assert_eq!(
            m.handle_key(&KeyCode::Char('s'), NONE),
            MenuOutcome::Pick(MenuChoice::ApproveTool {
                decision: D::AllowAlways
            })
        );
        assert_eq!(
            m.handle_key(&KeyCode::Char('d'), NONE),
            MenuOutcome::Pick(MenuChoice::ApproveTool {
                decision: D::RejectOnce
            })
        );
        assert_eq!(
            m.handle_key(&KeyCode::Char('x'), NONE),
            MenuOutcome::Pick(MenuChoice::ApproveTool {
                decision: D::RejectAlways
            })
        );
        // Esc is treated as deny-once by the loop (Cancel).
        assert_eq!(m.handle_key(&KeyCode::Escape, NONE), MenuOutcome::Cancel);
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

    /// Every menu overlay must render legibly: the title, item labels, the body
    /// note, and — the regression guard — the hotkey chips and the layer badge,
    /// which used the inverse `chip` on the dark `raise` surface (near-black on
    /// near-black). Covers both a selected and unselected row.
    #[test]
    fn menu_overlay_text_is_legible() {
        let mut m = confirm_menu(
            "Delete worktree?",
            "removes alpha from disk",
            "del",
            "0".into(),
            true,
        );
        m.handle_key(&KeyCode::Char('j'), NONE); // move the selection
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
        let v = crate::seg::text_contrast_violations(&mut s, 3.0);
        assert!(v.is_empty(), "low-contrast text in menu overlay: {v:?}");
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
