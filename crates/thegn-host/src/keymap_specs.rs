//! The native-host action registry: [`ActionSpec`] + the `ACTION_SPECS`
//! table that drives palette rows, compact hints, default chords, and the
//! discoverability tests. Extracted from `keymap.rs` (pinned by the file-size
//! ratchet); re-exported there so `crate::keymap::ACTION_SPECS` keeps working.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionSpec {
    /// Stable action id; matches [`crate::keymap::Action::key`] and command-palette dispatch.
    pub id: &'static str,
    /// Human label shown in the command palette and help surfaces.
    pub label: &'static str,
    /// Short label for compact bottom-bar hints.
    pub hint: &'static str,
    /// Built-in normal-mode/default chords. Config layers may override these.
    pub default_chords: &'static [&'static str],
    /// Whether this action should be surfaced in the command palette.
    pub palette: bool,
    /// Extra, **non-visible** search terms folded into the command-palette fuzzy
    /// haystack alongside `label` (synonyms, alternate names, related verbs) so an
    /// action surfaces even when the user types a word that isn't in its label —
    /// e.g. "fullscreen" / "maximize" / "full window" all reach `zoom`. Never
    /// rendered; matched only. **Every action MUST supply at least one keyword**
    /// (enforced by `tests::every_action_has_search_keywords`) — new actions are
    /// required to fill this in.
    pub keywords: &'static [&'static str],
}

/// Native-host action registry: one table drives palette rows, compact hints,
/// and tests that every real host action is discoverable. Keep ids aligned with
/// [`crate::keymap::Action::key`] / [`crate::keymap::Action::from_key`]; legacy aliases stay in `from_key`.
pub const ACTION_SPECS: &[ActionSpec] = &[
    ActionSpec {
        id: "new-worktree",
        label: "New worktree",
        hint: "worktree",
        default_chords: &["Alt w"],
        palette: true,
        keywords: &[
            "worktree",
            "branch",
            "new branch",
            "add worktree",
            "create worktree",
            "checkout",
        ],
    },
    ActionSpec {
        id: "new-workspace",
        label: "New workspace",
        hint: "workspace",
        default_chords: &["Alt W"],
        palette: true,
        keywords: &[
            "workspace",
            "repo",
            "repository",
            "project",
            "add repo",
            "open repo",
            "create workspace",
        ],
    },
    ActionSpec {
        id: "delete-workspace",
        label: "Delete workspace",
        hint: "remove the active workspace",
        // No default chord — `Alt X` / `Space X` belong to close-worktree, so
        // this action is palette-driven + user-bindable (also reachable from the
        // sidebar row menu's "Remove workspace").
        default_chords: &[],
        palette: true,
        keywords: &[
            "remove workspace",
            "delete repo",
            "close workspace",
            "drop workspace",
            "remove repo",
        ],
    },
    ActionSpec {
        id: "integrate",
        label: "Integrate (fold-actor)",
        hint: "integrate",
        // No default chord — palette-only + user-bindable, gated on
        // [merge_queue].enabled (see palette::build_command_palette_items).
        default_chords: &[],
        palette: true,
        keywords: &[
            "merge queue",
            "fold",
            "land",
            "merge",
            "integrate queue",
            "fold actor",
        ],
    },
    ActionSpec {
        id: "merge-drain",
        label: "Merge queue: drain (agent autopilot)",
        hint: "drain queue",
        // No default chord — palette + the section's `D`; gated on
        // [merge_queue].enabled like the other fold-actor actions.
        default_chords: &[],
        palette: true,
        keywords: &[
            "merge queue",
            "drain",
            "autopilot",
            "agent",
            "land all",
            "process queue",
        ],
    },
    ActionSpec {
        id: "new-tab",
        label: "New tab — same worktree",
        hint: "tab",
        default_chords: &["Alt t"],
        palette: true,
        keywords: &["tab", "new tab", "same worktree", "add tab"],
    },
    ActionSpec {
        id: "new-terminal",
        label: "New terminal",
        hint: "terminal",
        // `Alt T` — the capital T encodes Shift (see `sequence.rs`), so this is
        // physically Alt+Shift+T; `Alt t` (above) is the new-tab sibling. Registered
        // here so the chord surfaces in the keybindings help + the palette's chord
        // hint. `palette: false`: the wizard entry point is pushed explicitly (with
        // its `＋ …` styling) in `palette::build_command_palette_items`, so a
        // `palette: true` here would double-list it.
        default_chords: &["Alt T"],
        palette: false,
        keywords: &[
            "terminal",
            "new terminal",
            "shell",
            "session",
            "console",
            "add terminal",
        ],
    },
    ActionSpec {
        id: "new-pane",
        label: "New pane — smart split",
        hint: "smart split",
        default_chords: &["Alt p"],
        palette: true,
        keywords: &[
            "pane",
            "split",
            "new pane",
            "smart split",
            "divide",
            "window split",
        ],
    },
    ActionSpec {
        id: "split-down",
        label: "Split pane down",
        hint: "split↓",
        default_chords: &["Alt n"],
        palette: true,
        keywords: &[
            "split",
            "split down",
            "horizontal split",
            "pane below",
            "divide down",
            "stack panes",
        ],
    },
    ActionSpec {
        id: "split-right",
        label: "Split pane right",
        hint: "split→",
        default_chords: &["Alt N"],
        palette: true,
        keywords: &[
            "split",
            "split right",
            "vertical split",
            "pane beside",
            "divide right",
            "side by side",
        ],
    },
    ActionSpec {
        id: "close-pane",
        label: "Close pane",
        hint: "close pane",
        // No default chord: the smart `close` action (`Alt x`) closes the pane
        // when the tab is split. Kept explicit + rebindable for "always the
        // pane" semantics.
        default_chords: &[],
        palette: true,
        keywords: &[
            "close pane",
            "kill pane",
            "remove pane",
            "delete pane",
            "exit pane",
        ],
    },
    ActionSpec {
        id: "zoom",
        label: "Toggle zoom",
        hint: "zoom",
        default_chords: &["Ctrl Alt z"],
        palette: true,
        keywords: &[
            "zoom",
            "fullscreen",
            "full screen",
            "full window",
            "maximize",
            "maximise",
            "expand pane",
            "focus pane",
            "toggle fullscreen",
            "unzoom",
            "solo pane",
            "one pane",
        ],
    },
    ActionSpec {
        id: "redraw",
        label: "Redraw screen",
        hint: "redraw",
        default_chords: &["Ctrl Shift l"],
        palette: true,
        keywords: &[
            "redraw",
            "refresh",
            "repaint",
            "force redraw",
            "fix screen",
            "clear screen",
            "garbled",
            "corrupted display",
            "ctrl-l",
        ],
    },
    ActionSpec {
        id: "sync-panes",
        label: "Toggle sync-panes (broadcast input)",
        hint: "sync",
        default_chords: &["Ctrl Alt y"],
        palette: true,
        keywords: &[
            "sync panes",
            "broadcast",
            "broadcast input",
            "type to all",
            "mirror input",
            "send to all panes",
        ],
    },
    ActionSpec {
        id: "save-layout",
        label: "Save layout as…",
        hint: "save layout",
        default_chords: &[],
        palette: true,
        keywords: &["save layout", "store layout", "layout", "snapshot layout"],
    },
    ActionSpec {
        id: "apply-layout",
        label: "Apply saved layout…",
        hint: "apply layout",
        default_chords: &[],
        palette: true,
        keywords: &["apply layout", "load layout", "restore layout", "layout"],
    },
    ActionSpec {
        id: "export-layout",
        label: "Export layout to file…",
        hint: "export layout",
        default_chords: &[],
        palette: true,
        keywords: &[
            "export layout",
            "save layout to file",
            "layout file",
            "write layout",
        ],
    },
    ActionSpec {
        id: "import-layout",
        label: "Import layout from file…",
        hint: "import layout",
        default_chords: &[],
        palette: true,
        keywords: &[
            "import layout",
            "load layout from file",
            "layout file",
            "read layout",
        ],
    },
    ActionSpec {
        id: "new-worktree-from-template",
        label: "New worktree from template…",
        hint: "template",
        default_chords: &[],
        palette: true,
        keywords: &[
            "worktree",
            "template",
            "scaffold",
            "new worktree template",
            "from template",
        ],
    },
    ActionSpec {
        id: "cycle-theme",
        label: "Cycle theme",
        hint: "theme",
        default_chords: &["Ctrl Alt t"],
        palette: true,
        keywords: &[
            "theme",
            "color scheme",
            "colours",
            "cycle theme",
            "appearance",
            "dark mode",
            "light mode",
        ],
    },
    ActionSpec {
        id: "switch-font",
        // Single chord `Alt f`; the old redundant `Alt F` alias was dropped so
        // Shift keeps its "up-a-level" meaning everywhere in the Alt layer.
        label: "Switch font",
        hint: "font",
        default_chords: &["Alt f"],
        palette: true,
        keywords: &["font", "typeface", "switch font", "change font"],
    },
    ActionSpec {
        id: "close",
        label: "Close (pane or tab)",
        hint: "close",
        default_chords: &["Alt x"],
        palette: true,
        keywords: &[
            "close",
            "close pane",
            "close tab",
            "smart close",
            "exit",
            "kill",
        ],
    },
    ActionSpec {
        id: "close-tab",
        label: "Close tab",
        hint: "close tab",
        // No default chord: `Alt x` is the smart `close` above. Explicit +
        // rebindable for "close the tab specifically".
        default_chords: &[],
        palette: true,
        keywords: &[
            "close tab",
            "remove tab",
            "kill tab",
            "delete tab",
            "exit tab",
        ],
    },
    ActionSpec {
        id: "close-worktree",
        label: "Remove worktree",
        hint: "remove worktree (from disk)",
        default_chords: &["Alt X"],
        palette: true,
        keywords: &[
            "remove worktree",
            "delete worktree",
            "close worktree",
            "prune worktree",
            "drop worktree",
            "from disk",
        ],
    },
    ActionSpec {
        id: "switch-workspace",
        label: "Switch workspace",
        hint: "switch",
        default_chords: &["Alt o"],
        palette: true,
        keywords: &[
            "switch workspace",
            "change repo",
            "change workspace",
            "jump workspace",
            "select repo",
        ],
    },
    ActionSpec {
        id: "switch-account",
        label: "Switch agent account",
        hint: "account",
        // Palette-only: no default chord is installed by `default_keymap`.
        // A chord was declared here but never bound, so the hint strip,
        // `thegn keys list`, and the Keybindings page all advertised a key
        // that did nothing. Bind it by id in `[keybinds]`.
        default_chords: &[],
        palette: true,
        keywords: &[
            "switch account",
            "agent account",
            "change account",
            "login",
            "identity",
        ],
    },
    ActionSpec {
        id: "switch-bundle",
        label: "Switch env bundle",
        hint: "bundle",
        // Palette-only: no default chord is installed by `default_keymap`.
        // A chord was declared here but never bound, so the hint strip,
        // `thegn keys list`, and the Keybindings page all advertised a key
        // that did nothing. Bind it by id in `[keybinds]`.
        default_chords: &[],
        palette: true,
        keywords: &[
            "switch bundle",
            "env bundle",
            "environment bundle",
            "change bundle",
        ],
    },
    ActionSpec {
        id: "switch-profile",
        label: "Switch profile",
        hint: "profile",
        // Palette-only: no default chord is installed by `default_keymap`.
        // A chord was declared here but never bound, so the hint strip,
        // `thegn keys list`, and the Keybindings page all advertised a key
        // that did nothing. Bind it by id in `[keybinds]`.
        default_chords: &[],
        palette: true,
        keywords: &[
            "switch profile",
            "profile",
            "change profile",
            "select profile",
        ],
    },
    ActionSpec {
        id: "switch-identity",
        label: "Switch identity",
        hint: "identity",
        // Palette-only by default (reach via Ctrl+Space → "switch identity");
        // rebindable. Avoids clashing with the crowded Ctrl+Alt chord space.
        default_chords: &[],
        palette: true,
        keywords: &[
            "switch identity",
            "identity",
            "git identity",
            "change identity",
            "credentials",
            "mix and match",
        ],
    },
    ActionSpec {
        id: "prev-tab",
        label: "Previous tab",
        hint: "prev tab",
        // Alt+← reaches this via the seamless `nav-left` fall-through (at the
        // left edge with nowhere further to focus); kept chord-free + palette /
        // rebindable for a direct binding.
        default_chords: &[],
        palette: true,
        keywords: &[
            "previous tab",
            "prev tab",
            "back tab",
            "left tab",
            "earlier tab",
        ],
    },
    ActionSpec {
        id: "next-tab",
        label: "Next tab",
        hint: "next tab",
        // Reached via `nav-right` at the right edge; chord-free + rebindable.
        default_chords: &[],
        palette: true,
        keywords: &["next tab", "forward tab", "right tab", "later tab"],
    },
    ActionSpec {
        id: "prev-worktree",
        label: "Previous worktree",
        hint: "prev worktree",
        // Reached via `nav-up` at the top edge; chord-free + rebindable.
        default_chords: &[],
        palette: true,
        keywords: &[
            "previous worktree",
            "prev worktree",
            "back worktree",
            "up worktree",
            "earlier worktree",
        ],
    },
    ActionSpec {
        id: "next-worktree",
        label: "Next worktree",
        hint: "next worktree",
        // Reached via `nav-down` at the bottom edge; chord-free + rebindable.
        default_chords: &[],
        palette: true,
        keywords: &[
            "next worktree",
            "forward worktree",
            "down worktree",
            "later worktree",
        ],
    },
    ActionSpec {
        id: "prev-workspace",
        label: "Previous workspace",
        hint: "prev ws",
        default_chords: &["Shift Alt Up"],
        palette: true,
        keywords: &[
            "previous workspace",
            "prev workspace",
            "back workspace",
            "earlier repo",
        ],
    },
    ActionSpec {
        id: "next-workspace",
        label: "Next workspace",
        hint: "next ws",
        default_chords: &["Shift Alt Down"],
        palette: true,
        keywords: &["next workspace", "forward workspace", "later repo"],
    },
    ActionSpec {
        id: "toggle-region",
        label: "Toggle workspaces / terminals",
        hint: "ws↔term",
        default_chords: &["Alt `"],
        palette: true,
        keywords: &[
            "toggle region",
            "workspaces",
            "terminals",
            "switch region",
            "sidebar focus",
            "workspaces vs terminals",
        ],
    },
    ActionSpec {
        id: "move-item-up",
        label: "Move up (workspace/worktree)",
        hint: "move↑",
        default_chords: &["Ctrl Alt Up"],
        palette: true,
        keywords: &[
            "move up",
            "reorder up",
            "reorder workspace",
            "reorder worktree",
            "shift up",
            "promote",
        ],
    },
    ActionSpec {
        id: "move-item-down",
        label: "Move down (workspace/worktree)",
        hint: "move↓",
        default_chords: &["Ctrl Alt Down"],
        palette: true,
        keywords: &[
            "move down",
            "reorder down",
            "reorder workspace",
            "reorder worktree",
            "shift down",
            "demote",
        ],
    },
    ActionSpec {
        id: "move-worktree-to-folder",
        label: "Move worktree to folder…",
        hint: "file worktree",
        default_chords: &[],
        palette: true,
        keywords: &[
            "move worktree",
            "folder",
            "file worktree",
            "organize worktree",
            "group worktree",
        ],
    },
    ActionSpec {
        id: "focus-left",
        label: "Focus left",
        hint: "focus←",
        default_chords: &["Ctrl Left", "Ctrl h"],
        palette: true,
        keywords: &[
            "focus left",
            "move focus left",
            "pane left",
            "select left pane",
            "go left",
            "west pane",
        ],
    },
    ActionSpec {
        id: "focus-right",
        label: "Focus right",
        hint: "focus→",
        default_chords: &["Ctrl Right", "Ctrl l"],
        palette: true,
        keywords: &[
            "focus right",
            "move focus right",
            "pane right",
            "select right pane",
            "go right",
            "east pane",
        ],
    },
    ActionSpec {
        id: "focus-up",
        label: "Focus up",
        hint: "focus↑",
        default_chords: &["Ctrl Up", "Ctrl k"],
        palette: true,
        keywords: &[
            "focus up",
            "move focus up",
            "pane above",
            "select upper pane",
            "go up",
            "north pane",
        ],
    },
    ActionSpec {
        id: "focus-down",
        label: "Focus down",
        hint: "focus↓",
        default_chords: &["Ctrl Down", "Ctrl j"],
        palette: true,
        keywords: &[
            "focus down",
            "move focus down",
            "pane below",
            "select lower pane",
            "go down",
            "south pane",
        ],
    },
    // Nav = the seamless Alt+arrow motion: focus the neighbour (pane, then
    // sidebar / panel / bar / drawer); at the outer edge, fall through to the
    // tab (←/→) or worktree (↑/↓) switch. Resolved in the run loop.
    ActionSpec {
        id: "nav-left",
        label: "Navigate left",
        hint: "nav←",
        default_chords: &["Alt Left"],
        palette: true,
        keywords: &[
            "navigate left",
            "focus or previous tab",
            "pane left",
            "left edge previous tab",
            "seamless left",
        ],
    },
    ActionSpec {
        id: "nav-right",
        label: "Navigate right",
        hint: "nav→",
        default_chords: &["Alt Right"],
        palette: true,
        keywords: &[
            "navigate right",
            "focus or next tab",
            "pane right",
            "right edge next tab",
            "seamless right",
        ],
    },
    ActionSpec {
        id: "nav-up",
        label: "Previous worktree",
        hint: "wt↑",
        default_chords: &["Alt Up"],
        palette: true,
        keywords: &[
            "previous worktree",
            "cycle worktree up",
            "prior worktree in workspace",
        ],
    },
    ActionSpec {
        id: "nav-down",
        label: "Next worktree",
        hint: "wt↓",
        default_chords: &["Alt Down"],
        palette: true,
        keywords: &[
            "next worktree",
            "cycle worktree down",
            "following worktree in workspace",
        ],
    },
    ActionSpec {
        id: "toggle-sidebar",
        label: "Cycle sidebar: full / rail / hidden",
        hint: "sidebar",
        default_chords: &["Ctrl Alt s"],
        palette: true,
        keywords: &[
            "toggle sidebar",
            "hide sidebar",
            "show sidebar",
            "sidebar rail",
            "sidebar",
            "tree",
            "workspace tree",
        ],
    },
    ActionSpec {
        id: "warm-pool-increment",
        label: "Warm pool: add a spare",
        hint: "warm+",
        // Palette-only: no default chord is installed by `default_keymap`.
        // A chord was declared here but never bound, so the hint strip,
        // `thegn keys list`, and the Keybindings page all advertised a key
        // that did nothing. Bind it by id in `[keybinds]`.
        default_chords: &[],
        palette: true,
        keywords: &[
            "warm pool",
            "add spare",
            "prewarm",
            "spare worktree",
            "pool increment",
            "more spares",
        ],
    },
    ActionSpec {
        id: "warm-pool-decrement",
        label: "Warm pool: remove a spare",
        hint: "warm-",
        // Palette-only: no default chord is installed by `default_keymap`.
        // A chord was declared here but never bound, so the hint strip,
        // `thegn keys list`, and the Keybindings page all advertised a key
        // that did nothing. Bind it by id in `[keybinds]`.
        default_chords: &[],
        palette: true,
        keywords: &[
            "warm pool",
            "remove spare",
            "pool decrement",
            "fewer spares",
            "shrink pool",
        ],
    },
    ActionSpec {
        id: "toggle-panel",
        label: "Toggle diff / PR panel",
        hint: "panel",
        default_chords: &["Ctrl Alt p"],
        palette: true,
        keywords: &[
            "toggle panel",
            "diff panel",
            "pr panel",
            "show diff",
            "hide panel",
            "side panel",
        ],
    },
    ActionSpec {
        id: "files-drawer",
        label: "Toggle files drawer",
        hint: "drawer",
        default_chords: &["Ctrl Alt f"],
        palette: true,
        keywords: &[
            "files drawer",
            "toggle files",
            "file list",
            "changed files",
            "drawer",
        ],
    },
    ActionSpec {
        id: "toggle-corner",
        label: "Toggle corner overlay (video)",
        hint: "corner",
        default_chords: &["Ctrl Alt o"],
        palette: true,
        keywords: &[
            "corner overlay",
            "video",
            "picture in picture",
            "pip",
            "corner",
            "webcam",
        ],
    },
    ActionSpec {
        id: "focus-sidebar",
        label: "Focus workspace sidebar",
        hint: "sidebar",
        default_chords: &["Alt s"],
        palette: true,
        keywords: &[
            "focus sidebar",
            "sidebar",
            "jump to sidebar",
            "workspace tree",
            "select sidebar",
        ],
    },
    ActionSpec {
        id: "focus-panel",
        label: "Focus diff / PR panel",
        hint: "panel",
        default_chords: &["Alt ."],
        palette: true,
        keywords: &[
            "focus panel",
            "diff panel",
            "pr panel",
            "jump to panel",
            "select panel",
        ],
    },
    ActionSpec {
        id: "toggle-notifications",
        label: "Toggle Notifications panel",
        hint: "notifications",
        default_chords: &["Ctrl Alt i"],
        palette: true,
        keywords: &[
            "notifications",
            "toggle notifications",
            "notification panel",
            "alerts",
            "inbox",
        ],
    },
    ActionSpec {
        id: "open-ci",
        label: "Open CI/CD runs panel",
        hint: "ci",
        default_chords: &[],
        palette: true,
        keywords: &[
            "ci",
            "cicd",
            "ci/cd",
            "runs",
            "pipelines",
            "actions",
            "checks",
            "workflows",
            "builds",
        ],
    },
    ActionSpec {
        id: "open-usage",
        label: "AI account usage",
        hint: "usage",
        default_chords: &["Alt u"],
        palette: true,
        keywords: &[
            "usage",
            "account usage",
            "rate limit",
            "rate limits",
            "quota",
            "limits",
            "orca",
            "session weekly monthly",
            "claude codex antigravity",
        ],
    },
    ActionSpec {
        id: "open-monitor",
        label: "System monitor",
        hint: "monitor",
        default_chords: &["Ctrl Alt M"],
        palette: true,
        keywords: &[
            "monitor",
            "system monitor",
            "resources",
            "cpu",
            "memory",
            "ram",
            "swap",
            "processes",
            "top htop btop",
            "temperature thermal",
            "gpu vram",
            "network throughput",
            "disk io",
            "battery power",
            "graphs history",
        ],
    },
    ActionSpec {
        id: "open-calendar",
        label: "Calendar & world clocks",
        hint: "calendar",
        default_chords: &["Alt d"],
        palette: true,
        keywords: &[
            "calendar",
            "date",
            "clock",
            "time",
            "month",
            "agenda",
            "schedule",
            "events",
            "world clock",
            "timezone",
            "time zone",
            "tz",
            "utc",
            "meetings",
            "today",
            "diary",
        ],
    },
    ActionSpec {
        id: "open-merge-queue",
        label: "Merge queue",
        hint: "merge queue",
        // Palette-only: no default chord is installed by `default_keymap`.
        // A chord was declared here but never bound, so the hint strip,
        // `thegn keys list`, and the Keybindings page all advertised a key
        // that did nothing. Bind it by id in `[keybinds]`.
        default_chords: &[],
        palette: true,
        keywords: &[
            "merge queue",
            "queue",
            "folds",
            "landing",
            "merge",
            "integrate",
        ],
    },
    ActionSpec {
        id: "open-pr-queue",
        label: "PR queue",
        hint: "pr queue",
        // No default chord: `Ctrl Alt q` is the merge queue's, and the two are
        // distinct features. Reachable from the palette and the panel.
        default_chords: &[],
        palette: true,
        keywords: &[
            "pr queue",
            "pull request queue",
            "queue",
            "review",
            "checks",
            "team",
            "babysit",
        ],
    },
    ActionSpec {
        id: "pr-queue-add",
        label: "PR queue: watch this PR",
        hint: "watch pr",
        default_chords: &[],
        palette: true,
        keywords: &[
            "pr queue add",
            "watch pr",
            "queue pull request",
            "enqueue pr",
            "shepherd",
        ],
    },
    ActionSpec {
        id: "pr-queue-refresh",
        label: "PR queue: refresh now",
        hint: "refresh prs",
        default_chords: &[],
        palette: true,
        keywords: [
            "pr queue refresh",
            "pr queue drain",
            "poll pull requests",
            "check prs",
            "refresh pr queue",
        ]
        .as_slice(),
    },
    ActionSpec {
        id: "share-worktree-port",
        label: "Share worktree port",
        hint: "share",
        default_chords: &["Alt Shift S"],
        palette: true,
        keywords: &[
            "share port",
            "expose port",
            "tunnel",
            "share worktree",
            "publish port",
            "forward port",
        ],
    },
    ActionSpec {
        id: "stop-worktree-share",
        label: "Stop worktree shares",
        hint: "unshare",
        default_chords: &[],
        palette: true,
        keywords: &[
            "stop share",
            "unshare",
            "close tunnel",
            "stop port",
            "revoke share",
        ],
    },
    ActionSpec {
        id: "open-shares",
        label: "Open shares panel",
        hint: "shares",
        default_chords: &[],
        palette: true,
        keywords: &[
            "shares panel",
            "shared ports",
            "tunnels",
            "open shares",
            "shares",
        ],
    },
    ActionSpec {
        id: "palette",
        label: "Command palette",
        hint: "menu",
        default_chords: &["Ctrl Space"],
        palette: true,
        keywords: &[
            "command palette",
            "menu",
            "commands",
            "cmdk",
            "command k",
            "actions",
            "run command",
            "search commands",
        ],
    },
    ActionSpec {
        id: "help",
        label: "Help — built-in docs",
        hint: "help",
        default_chords: &["F1"],
        palette: true,
        keywords: &[
            "help",
            "docs",
            "documentation",
            "manual",
            "keybindings",
            "shortcuts",
            "guide",
            "about",
        ],
    },
    ActionSpec {
        id: "lazygit",
        label: "Open lazygit",
        hint: "lazygit",
        default_chords: &["Alt g"],
        palette: true,
        keywords: &[
            "lazygit",
            "git ui",
            "git tui",
            "git",
            "commit",
            "stage",
            "git client",
        ],
    },
    ActionSpec {
        id: "yazi",
        label: "Open yazi drawer",
        hint: "yazi",
        default_chords: &["Alt y"],
        palette: true,
        keywords: &[
            "yazi",
            "file manager",
            "file browser",
            "files",
            "explorer",
            "drawer",
        ],
    },
    ActionSpec {
        id: "editor",
        label: "Open editor",
        hint: "editor",
        default_chords: &["Alt e"],
        palette: true,
        keywords: &[
            "editor",
            "open editor",
            "edit",
            "vim",
            "nvim",
            "code",
            "text editor",
        ],
    },
    ActionSpec {
        id: "show-diff",
        label: "Open git diff",
        hint: "diff",
        default_chords: &["Alt /"],
        palette: true,
        keywords: &[
            "diff",
            "git diff",
            "changes",
            "show diff",
            "unstaged",
            "review changes",
        ],
    },
    ActionSpec {
        id: "git-push",
        label: "Git push (current branch)",
        hint: "push",
        default_chords: &[],
        palette: true,
        keywords: &[
            "git push",
            "push",
            "upload",
            "publish branch",
            "push branch",
        ],
    },
    ActionSpec {
        id: "git-pull",
        label: "Git pull (current branch)",
        hint: "pull",
        default_chords: &[],
        palette: true,
        keywords: &[
            "git pull",
            "pull",
            "sync",
            "update branch",
            "fetch and merge",
        ],
    },
    ActionSpec {
        id: "git-fetch",
        label: "Git fetch (all remotes, prune)",
        hint: "fetch",
        default_chords: &[],
        palette: true,
        keywords: &[
            "git fetch",
            "fetch",
            "prune",
            "refresh remotes",
            "update remotes",
        ],
    },
    ActionSpec {
        id: "rollback",
        label: "Rollback / discard changes…",
        hint: "rollback",
        default_chords: &[],
        palette: true,
        keywords: &[
            "rollback",
            "discard",
            "revert",
            "undo changes",
            "reset",
            "restore",
            "discard changes",
        ],
    },
    ActionSpec {
        id: "scroll-up",
        label: "Scroll pane up",
        hint: "scroll↑",
        default_chords: &["Shift PageUp", "PageUp"],
        palette: true,
        keywords: &[
            "scroll up",
            "page up",
            "scrollback up",
            "history up",
            "back scroll",
        ],
    },
    ActionSpec {
        id: "scroll-down",
        label: "Scroll pane down",
        hint: "scroll↓",
        default_chords: &["Shift PageDown", "PageDown"],
        palette: true,
        keywords: &[
            "scroll down",
            "page down",
            "scrollback down",
            "history down",
            "forward scroll",
        ],
    },
    ActionSpec {
        id: "copy-pane",
        label: "Copy pane contents",
        hint: "copy",
        default_chords: &["Ctrl Alt c"],
        palette: true,
        keywords: &[
            "copy pane",
            "copy",
            "yank",
            "copy contents",
            "grab output",
            "clipboard",
        ],
    },
    ActionSpec {
        id: "search-pane",
        label: "Search pane history",
        hint: "search",
        // Must match what `default_keymap` actually binds. A bare `/` was
        // declared here for a long time while dispatch bound `Ctrl Alt /` —
        // single-key chords are prevented by rule (see the comment beside the
        // `insert_all` in `keymap.rs`), so the hint, `thegn keys list`, and the
        // generated Keybindings page were all advertising a key that does
        // nothing.
        default_chords: &["Ctrl Alt /"],
        palette: true,
        keywords: &[
            "search pane",
            "find",
            "search history",
            "search scrollback",
            "grep pane",
            "find in pane",
        ],
    },
    ActionSpec {
        id: "search-global",
        label: "Search across all panes (worktree scope)",
        hint: "search all",
        default_chords: &["Ctrl /"],
        palette: true,
        keywords: &[
            "search all panes",
            "global search",
            "find across panes",
            "search worktree",
            "grep all",
        ],
    },
    ActionSpec {
        id: "toggle-key-lock",
        label: "Lock/unlock keybinds (pass through)",
        hint: "lock",
        default_chords: &["Ctrl g"],
        palette: true,
        keywords: &[
            "key lock",
            "lock keys",
            "passthrough",
            "pass through",
            "unlock keybinds",
            "disable keybinds",
            "raw input",
        ],
    },
    ActionSpec {
        id: "mode-normal",
        label: "Switch to Normal mode",
        hint: "normal",
        default_chords: &["Ctrl Alt n"],
        palette: true,
        keywords: &["normal mode", "mode", "default mode", "switch mode"],
    },
    ActionSpec {
        id: "mode-vim-normal",
        label: "Switch to Vim-normal mode",
        hint: "vim",
        default_chords: &["Ctrl Alt v"],
        palette: true,
        keywords: &["vim normal", "vim mode", "vi mode", "modal", "vim"],
    },
    ActionSpec {
        id: "mode-vim-insert",
        label: "Switch to Vim-insert mode",
        hint: "insert",
        default_chords: &[],
        palette: true,
        keywords: &["vim insert", "insert mode", "vim", "vi insert"],
    },
    ActionSpec {
        id: "mode-emacs",
        label: "Switch to Emacs mode",
        hint: "emacs",
        default_chords: &["Ctrl Alt e"],
        palette: true,
        keywords: &["emacs mode", "emacs", "readline mode"],
    },
    ActionSpec {
        id: "toggle-strip",
        label: "Toggle pin strip",
        hint: "pins",
        default_chords: &["Ctrl Alt b"],
        palette: true,
        keywords: &[
            "pin strip",
            "pins",
            "toggle pins",
            "show pins",
            "hide pins",
            "strip",
        ],
    },
    ActionSpec {
        id: "grow-strip",
        label: "Grow pin strip",
        hint: "pins+",
        default_chords: &["Ctrl Alt ]"],
        palette: true,
        keywords: &[
            "grow pins",
            "bigger pin strip",
            "expand strip",
            "pin strip larger",
            "wider pins",
        ],
    },
    ActionSpec {
        id: "shrink-strip",
        label: "Shrink pin strip",
        hint: "pins-",
        default_chords: &["Ctrl Alt ["],
        palette: true,
        keywords: &[
            "shrink pins",
            "smaller pin strip",
            "contract strip",
            "pin strip smaller",
            "narrower pins",
        ],
    },
    ActionSpec {
        id: "promote-pin",
        label: "Promote pane to pin strip",
        hint: "pin pane",
        default_chords: &["Ctrl Alt P"],
        palette: true,
        keywords: &[
            "pin pane",
            "promote pin",
            "add pin",
            "pin this pane",
            "move to pins",
        ],
    },
    ActionSpec {
        id: "unpin",
        label: "Unpin focused/first pin",
        hint: "unpin",
        default_chords: &["Ctrl Alt U"],
        palette: true,
        keywords: &["unpin", "remove pin", "drop pin", "detach pin"],
    },
    ActionSpec {
        id: "quit",
        label: "Quit thegn",
        hint: "quit",
        default_chords: &["Ctrl q"],
        palette: true,
        keywords: &["quit", "exit", "close app", "leave", "shutdown"],
    },
    // The persistent-lifecycle pair (daemon-backed panes): quit is already a
    // detach; these make the two intents explicit and palette-discoverable.
    ActionSpec {
        id: "detach",
        label: "Detach — quit, keep panes running",
        hint: "detach",
        default_chords: &[],
        palette: true,
        keywords: &[
            "detach",
            "quit keep running",
            "background",
            "leave panes running",
            "disconnect",
        ],
    },
    ActionSpec {
        id: "quit-kill",
        label: "Quit and kill sessions",
        hint: "quit+kill",
        default_chords: &[],
        palette: true,
        keywords: &[
            "quit and kill",
            "kill sessions",
            "exit and kill",
            "force quit",
            "shutdown panes",
        ],
    },
    // Media transport (optional [media] feature). Leader: `Alt m`. All inert when
    // media is disabled; surfaced in the palette so they're discoverable.
    ActionSpec {
        id: "media-play-pause",
        label: "Media: Play/Pause",
        hint: "play/pause",
        default_chords: &["Alt m m"],
        palette: true,
        keywords: &[
            "media",
            "play",
            "pause",
            "play pause",
            "music",
            "toggle playback",
        ],
    },
    ActionSpec {
        id: "media-next",
        label: "Media: Next track",
        hint: "next",
        default_chords: &["Alt m n"],
        palette: true,
        keywords: &[
            "media",
            "next track",
            "skip track",
            "next song",
            "forward track",
        ],
    },
    ActionSpec {
        id: "media-previous",
        label: "Media: Previous track",
        hint: "prev",
        default_chords: &["Alt m p"],
        palette: true,
        keywords: &[
            "media",
            "previous track",
            "prev track",
            "back track",
            "previous song",
        ],
    },
    ActionSpec {
        id: "media-shuffle-toggle",
        label: "Media: Toggle shuffle",
        hint: "shuffle",
        default_chords: &["Alt m s"],
        palette: true,
        keywords: &["media", "shuffle", "random", "toggle shuffle"],
    },
    ActionSpec {
        id: "media-loop-cycle",
        label: "Media: Cycle repeat",
        hint: "loop",
        default_chords: &["Alt m r"],
        palette: true,
        keywords: &["media", "repeat", "loop", "cycle repeat", "repeat mode"],
    },
    ActionSpec {
        id: "media-volume-up",
        label: "Media: Volume up",
        hint: "vol+",
        default_chords: &["Alt m k"],
        palette: true,
        keywords: &["media", "volume up", "louder", "raise volume", "vol up"],
    },
    ActionSpec {
        id: "media-volume-down",
        label: "Media: Volume down",
        hint: "vol−",
        default_chords: &["Alt m j"],
        palette: true,
        keywords: &[
            "media",
            "volume down",
            "quieter",
            "lower volume",
            "vol down",
        ],
    },
    ActionSpec {
        id: "media-seek-forward",
        label: "Media: Skip forward",
        hint: "seek+",
        default_chords: &["Alt m ."],
        palette: true,
        keywords: &[
            "media",
            "seek forward",
            "skip ahead",
            "fast forward",
            "scrub forward",
        ],
    },
    ActionSpec {
        id: "media-seek-back",
        label: "Media: Skip back",
        hint: "seek−",
        default_chords: &["Alt m ,"],
        palette: true,
        keywords: &["media", "seek back", "rewind", "skip back", "scrub back"],
    },
    ActionSpec {
        id: "media-open-panel",
        label: "Media: Now-Playing panel…",
        hint: "now playing",
        default_chords: &["Alt m enter"],
        palette: true,
        keywords: &[
            "media",
            "now playing",
            "media panel",
            "player panel",
            "music panel",
        ],
    },
    ActionSpec {
        id: "media-chapter-next",
        label: "Media: Next chapter (video)",
        hint: "chapter+",
        default_chords: &["Alt m ]"],
        palette: true,
        keywords: &["media", "next chapter", "chapter forward", "video chapter"],
    },
    ActionSpec {
        id: "media-chapter-prev",
        label: "Media: Previous chapter (video)",
        hint: "chapter−",
        default_chords: &["Alt m ["],
        palette: true,
        keywords: &["media", "previous chapter", "chapter back", "video chapter"],
    },
    ActionSpec {
        id: "media-fullscreen",
        label: "Media: Toggle fullscreen (video)",
        hint: "fullscreen",
        default_chords: &["Alt m v"],
        palette: true,
        keywords: &[
            "media",
            "fullscreen video",
            "full screen",
            "maximize video",
            "video fullscreen",
        ],
    },
    ActionSpec {
        id: "media-select-playlist",
        label: "Media: Select playlist…",
        hint: "playlist",
        default_chords: &["Alt m l"],
        palette: true,
        keywords: &["media", "playlist", "select playlist", "choose playlist"],
    },
    ActionSpec {
        id: "media-select-player",
        label: "Media: Select player…",
        hint: "player",
        default_chords: &["Alt m o"],
        palette: true,
        keywords: &[
            "media",
            "player",
            "select player",
            "choose player",
            "output device",
        ],
    },
    // Notification routing (items 426/427).
    ActionSpec {
        id: "notify-dnd-toggle",
        label: "Notifications: Toggle do-not-disturb",
        hint: "dnd",
        default_chords: &["Ctrl Alt d"],
        palette: true,
        keywords: &[
            "do not disturb",
            "dnd",
            "silence",
            "mute notifications",
            "quiet",
        ],
    },
    ActionSpec {
        id: "notify-mode-cycle",
        label: "Notifications: Cycle routing mode",
        hint: "notif mode",
        default_chords: &["Ctrl Alt m"],
        palette: true,
        keywords: &[
            "notification mode",
            "routing mode",
            "cycle notifications",
            "notify mode",
        ],
    },
    ActionSpec {
        id: "attention-next",
        label: "Jump to next attention",
        hint: "needs you",
        default_chords: &["Alt a"],
        palette: true,
        keywords: &[
            "attention",
            "needs you",
            "next attention",
            "jump to alert",
            "waiting",
            "needs input",
        ],
    },
    ActionSpec {
        id: "mark-all-read",
        label: "Notifications: Mark all as read",
        hint: "mark all read",
        default_chords: &["Alt Shift R"],
        palette: true,
        keywords: &[
            "mark all read",
            "clear notifications",
            "read all",
            "dismiss all",
            "clear inbox",
        ],
    },
];

pub fn action_specs() -> &'static [ActionSpec] {
    ACTION_SPECS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every action MUST carry at least one non-empty search keyword — this is
    /// the "new ones are required" gate. The command palette folds `keywords`
    /// into its fuzzy haystack (see `palette::build_command_palette_items`), so a
    /// missing keyword set silently narrows discoverability. Adding an action
    /// without keywords fails here.
    #[test]
    fn every_action_has_search_keywords() {
        for spec in ACTION_SPECS {
            assert!(
                !spec.keywords.is_empty(),
                "action `{}` has no search keywords — add synonyms/alternate \
                 names so it surfaces in the command palette",
                spec.id
            );
            assert!(
                spec.keywords.iter().all(|k| !k.trim().is_empty()),
                "action `{}` has an empty/whitespace keyword",
                spec.id
            );
        }
    }

    /// `default_chords` must be what the keymap actually binds.
    ///
    /// It is a *display* field — `chord_hint_for` reads it for the status-bar
    /// hints, `thegn keys list`, and the generated Keybindings help page —
    /// while dispatch comes from `default_keymap`. Nothing kept the two
    /// honest, and `search-pane` drifted: it advertised a bare `/` for a long
    /// time while dispatch bound `Ctrl Alt /`, so every surface published a key
    /// that did nothing.
    #[test]
    fn declared_default_chords_actually_dispatch() {
        use crate::keymap::{Action, Mode, parse_chord};
        use crate::sequence::MatchResult;
        let mut wrong: Vec<String> = Vec::new();
        for spec in ACTION_SPECS {
            let Some(chord) = spec.default_chords.first() else {
                continue;
            };
            let Ok(keys) = parse_chord(chord) else {
                wrong.push(format!("{}: `{chord}` does not parse", spec.id));
                continue;
            };
            // Feed the chord through a fresh default map and see what it fires.
            let mut map = crate::keymap::default_keymap();
            let mut got: Option<Action> = None;
            for k in keys {
                match map.dispatch(Mode::Normal, k) {
                    MatchResult::Matched(a) => {
                        got = Some(a);
                        break;
                    }
                    MatchResult::Pending => continue,
                    MatchResult::None => break,
                }
            }
            match got {
                // Some ids are aliases onto a shared Action (e.g. the tool
                // launchers), so compare the resolved action's key, not the id.
                Some(a) if a.key() == spec.id => {}
                Some(a) => wrong.push(format!(
                    "{}: declares `{chord}`, but that fires `{}`",
                    spec.id,
                    a.key()
                )),
                None => wrong.push(format!("{}: declares `{chord}`, which is unbound", spec.id)),
            }
        }
        assert!(
            wrong.is_empty(),
            "default_chords disagree with what default_keymap binds:\n  {}\n\
             Fix the spec (or the binding) — these strings are published to \
             users as real keys.",
            wrong.join("\n  ")
        );
    }
}
