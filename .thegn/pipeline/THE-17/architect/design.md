# THE-17 — Deep external IDE integration

Status: architecture handoff for implementation

## Outcome

Add one external-editor handoff path that can open either a selected worktree
or a worktree-relative file at a line/column in a configured IDE. The same
core request and the same host launcher are used by:

- a sidebar worktree-row action;
- a diff-view file/hunk action;
- a PR-comment thread action;
- a palette action; and
- the `editor.open` control/MCP capability.

The handoff is local to the owning thegn compositor. A control or MCP caller
may request a target, but never supplies an executable, argv, provider, or
environment. The daemon records a short-lived intent; the compositor validates
and launches it with the selected local provider.

The implementation extends the existing `thegn_core::editor::Editor` seam.
It does not add a second IDE abstraction, an IDE-specific UI protocol, a PR
comment model, or a URL scheme.

## Standards and invariants applied

The governing requirements are `CLAUDE.md:30-58` and
`docs/ARCHITECTURE.md:30-37`: core remains substrate-free, backends are seams,
the control surfaces project one catalog, and git/forge/SQLite ownership is
preserved. The event-loop requirements at
`docs/ARCHITECTURE.md:54-84` are binding: no `which`, filesystem probing,
subprocess creation, sandbox probing, or child waiting on the loop or before
the first frame. The provider shape and reserved-operation rules are
`docs/ARCHITECTURE.md:110-149`; capability and surface rules are
`docs/ARCHITECTURE.md:151-180`; config and ratchets are
`docs/ARCHITECTURE.md:199-214`; action/help rules are
`docs/ARCHITECTURE.md:216-228`.

The detached process must retain the existing reaping contract in
`crates/thegn-host/src/actions.rs:140-163` and add the CPU-cap wrapper from
`crates/thegn-core/src/sandbox_cpucap.rs:610-680`. Since the wrapper may probe
the platform, wrapping and spawning happen in the detached/off-loop worker,
not in the UI action handler.

## Existing seam audit (verified on this branch)

`crates/thegn-core/src/editor.rs:23-45` currently models only a file path plus
optional line/column and returns a shell command with pane/external placement.
`EditorCaps` at `editor.rs:47-56` reports line, column, and external placement;
`Editor::open` at `editor.rs:89-95` is synchronous and planning-only. The
program table at `editor.rs:101-185` already knows line syntaxes for several
terminal and GUI programs. `ProgramEditor::probe` at `editor.rs:249-274`
does a PATH check, and the doctor registry calls the selected editor from
`crates/thegn-svc/src/seam/registry.rs:334-343`; that probe is a CLI/doctor
operation, not a TUI-loop service.

What it covers:

| Concern                 | Current result                                                                                                                                       | Design consequence                                                                           |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| Open file               | Yes, through `OpenRequest` and `Editor::open`                                                                                                        | Preserve and broaden the request rather than adding an IDE helper.                           |
| Open directory/worktree | No; no directory operation or target policy                                                                                                          | Add an optional directory/project operation with `Unsupported` when a provider cannot do it. |
| Line/column             | Yes, but only for the static `program_profile` table                                                                                                 | Move argv shaping into provider implementations and test each advertised capability.         |
| Installed-IDE detection | Only the selected generic program is probed; the doctor path is the existing seam registry                                                           | Register all providers with cheap doctor probes; never probe from an event-loop action.      |
| Per-workspace default   | No. `Config::tool_command` is global (`crates/thegn-core/src/config.rs:6442-6447`) and `WorkspaceConfig` has no editor field (`config.rs:2251-2305`) | Add a trusted workspace provider override and an effective resolver.                         |
| Launch safety           | `panel_util::spawn_editor_detached` directly creates a child (`crates/thegn-host/src/panel_util.rs:29-39`)                                           | Replace this path with an off-loop argv + `sandbox_cpucap` + reaped spawn.                   |

There are also existing host callers that bypass the seam: the search result
path in `crates/thegn-host/src/run.rs:15720+` and the Ctrl-O path around
`run.rs:17985-18014`. The implementation must route the handoff surfaces and
these adjacent file-open paths through the shared helper, with no new direct
`EDITOR`/shell parsing.

## Provider design

Keep `thegn_core::editor::Editor` object-safe, synchronous, and planning-only.
Refactor its launch result from a shell-only command into a structured argv
plan (the host may render argv for a terminal-pane command at the edge):

- `EditorTarget`: trusted absolute worktree plus an optional validated,
  worktree-relative file and optional 1-based line/column;
- `EditorLaunch`: argv, working directory, placement, provider id, and the
  requested operation; and
- `EditorCaps`: `open_file`, `open_directory`, `line`, `column`, and
  `external` (or equivalent names), with every advertised capability backed
  by an implementation and every unsupported operation returning
  `EditorError::Unsupported`.

The pure target policy rejects an empty worktree, rejects a relative path that
escapes it (`..` after normalization), rejects line/column without a file, and
uses the worktree root when the request is project-only. It accepts only
worktree-relative paths from diff/PR/control callers; no caller can pass an
arbitrary absolute file outside the selected worktree. A missing or stale
file is an edge failure after target validation, reported in the UI rather
than panicking.

Add provider implementation modules under `crates/thegn-core/src/editor/`:

- `vscode.rs`, `cursor.rs`, `zed.rs`, `jetbrains.rs`, `nvim_remote.rs`, and
  `emacs.rs` implement the logical provider kinds requested by the config;
- `providers.rs` owns registration, kind dispatch, and doctor probe assembly;
- the existing template/custom-program provider remains for
  `[editor] command`, `[[tools]] editor`, `$VISUAL`, `$EDITOR`, and the `vi`
  fallback, but uses the structured launch result.

Executable/vendor CLI spellings are constants in the corresponding provider
implementation module only. They must not appear in `config.rs`, the catalog,
control JSON, help, or target-selection code. Provider IDs are logical config
values (`vscode`, `cursor`, `zed`, `jetbrains`, `nvim_remote`, `emacs`). A
provider that cannot open a project or cannot express a column reports that
operation as unsupported; the UI falls back to file-only or line-only with a
visible status message. No fake “best effort” capability is advertised.

`auto` preserves the existing custom-program ladder and does not scan the PATH
on a UI action. `thegn doctor` enumerates all registered providers via the
existing `Probe` registry, off the TUI loop, and reports ready/unavailable/
reserved plus caps. An explicitly configured provider is planned without a
launch-time probe; a failed spawn is an edge error. This makes detection
honest and keeps the first frame and idle loop free of subprocess I/O.

Config additions:

```toml
[editor]
provider = "auto"       # auto | vscode | cursor | zed | jetbrains | nvim_remote | emacs
command = ""            # existing trusted custom template; still wins when non-empty
open_in = "auto"         # existing placement override

[workspace.my-repo]
editor = "cursor"        # optional logical provider override; omitted = global provider
```

The exact serde field may be named `editor_provider` if that avoids confusing
the existing `[editor]` table, but the public TOML shape must be documented as
shown and tested. Global precedence is: non-empty trusted `[editor] command`,
workspace provider override, global provider, then the existing custom-program
environment ladder. The workspace override never reads repo-local `.thegn.*`:
launching an IDE is a trusted user-config operation. Add
`THEGN_EDITOR_PROVIDER` to the env overlay and its coverage test; the dynamic
`[workspace.<slug>]` override is config-file-only like the other workspace
maps. Document both keys in `config/config.toml.example` and the configuration
help page.

## Host handoff and state flow

Introduce a small host `ide_handoff` module rather than growing `run.rs` or
`panel_util.rs` into a god file. It accepts the core `EditorTarget`, resolves
the effective config for the selected worktree, and has two phases:

1. A pure/request phase returns a target and provider plan, suitable for unit
   tests and for all UI/control callers.
2. An off-loop launch phase performs any provider selection/probe required by
   `auto`, wraps the argv with `sandbox_cpucap::wrap_background_argv`, spawns
   with the worktree as cwd, and reaps the child through the existing action
   helper. It sends success/failure back on the normal host channel and pulses
   `TerminalWaker`. Pane placement is handled at the edge by converting the
   argv to the existing terminal command path; external placement is detached.

The daemon `ControlApi::open_editor` only writes an `open_editor` intent to
the existing SQLite mailbox, just as `open_worktree` writes
`focus_workspace` (`crates/thegn-host/src/daemon/service.rs:743-755`). Add the
intent carrier to hydration/`FrameModel`, claim-and-delete it in the existing
hydration pass, and drain it before the model swap. The compositor owns the
actual launch and can therefore use its own effective config and UI channels.
No migration is needed: this is a new intent kind in the existing mailbox.
Intent payload fields are worktree, optional relative path, optional line and
column, and a source label for diagnostics. Never serialize argv or a provider
chosen by a remote caller.

## Surface behavior

### Sidebar

The worktree-row context menu gets `Open in IDE`. It uses the selected row’s
worktree path, including dormant/non-active rows, and does not silently switch
the active tab. If the row has no resolvable worktree path, the menu entry is
omitted or returns a status error. The menu action calls the shared handoff
helper.

### Diff view

Add an explicit `Open in IDE` action for the selected file/hunk line. For a
selected added/context line use the new-side 1-based line; for a file row use
the file with no line. Deleted lines have no valid new-side anchor, so fall
back to the file and explain that the deleted line cannot be opened exactly.
Do not use the structural-diff flat pane, which has no stable file target.

### PR comments

This is serial after THE-27 (`tg/the-27-pr-comments-in-diff`). Consume its
`PrReviewSnapshot`, anchored thread rows, and `path`/`new_lineno` projection.
Add only the handoff action/result to that existing projection. Do not add a
new thread type, parser, cache table, comment identity, or anchor algorithm.
If a thread has no resolvable path/line, open the worktree and show a status
message. THE-27’s exact anchor semantics remain authoritative.

### Palette and key/help

Add a new `OpenInIde` action with id `open-in-ide` to `Action`, `ACTION_SPECS`,
and palette. It has no default chord unless a free chord is deliberately
ratified; the diff/PR/sidebar local actions may expose their own explicit
entry. The action operates on the focused worktree, or reports a clear
“select a worktree” status. Keep the existing `editor` action for a terminal
editor tool; do not overload it.

Claim the new action in `docs/help/terminal-and-panes.md` (and mention the
sidebar/diff/PR variants in the relevant pages) so all help ratchets remain
empty. Add the config reference text through the existing generated help
path, not a second config documentation source.

## Control/MCP capability

Add one catalog row `editor.open`, mapped to a new `Verb::OpenEditor` with
`write` scope and all applicable surfaces. The operation is a queued local
handoff, so its request is:

```json
{
  "worktree": "/absolute/worktree",
  "path": "src/main.rs",
  "line": 42,
  "col": 7
}
```

`worktree` is required; `path`, `line`, and `col` are optional; unknown fields
are rejected. The result is an acknowledgement that the intent was queued,
not a claim that an IDE launched. Reuse the catalog’s scope/audit path and
the generic `API_CALLS` path `/v1/editor/open`; do not add a vendor-specific
control method.

Implement the same operation in HTTP, gRPC, CLI generic `thegn api call`, MCP
state tools, and plugin host-call projection. Add the control wire type and
route to `docs/api/control-v1.json` using the existing additive snapshot
workflow. MCP exposes `editor.open` with the same four arguments and write
scope. The existing completion classification for generic `api call params`
(`test/completion-slot-ratchet.txt`) remains the one CLI slot; no new
value-taking CLI command is justified. Run the completion-slot ratchet and
keep it unchanged rather than adding an excuse.

Because all surfaces are implemented in this change, do not add an entry to
`SURFACE_GAPS`; remove no unrelated excuses. Add the catalog row, route,
`GRPC_CAPS`, `MCP_STATE_CAPS`, and derived CLI coverage in the same dependency
chain so the surface ledger reaches zero new gaps.

## Draft openspec disposition

The existing `openspec/changes/add-external-ide-handoff/` draft was read before
this design — `proposal.md`, `design.md`, `tasks.md`, and every
`specs/*/spec.md` under that change. Its useful substrate claim is confirmed:
the editor seam exists,
and its file/line/column planning plus selected-program doctor probe already
landed. Its claim that per-workspace `[[tools]] editor` is already supported
is false on this branch: `Config::tool_command` is global and
`WorkspaceConfig` has no editor field. The draft’s proposed project-level
operation is retained in the seam, with explicit unsupported capabilities at
providers that cannot perform it.

Cut from the draft: `thegn://` URL registration/dispatch, macOS launcher
artifacts, IDE-side extension recipes, and a separate `thegn open --file`
command. None is in the issue’s required handoff surfaces; each would create a
second inbound protocol or platform integration before the local seam,
control capability, and UI actions are complete. The PR-comment work is
explicitly delegated to THE-27’s data model rather than duplicated here.

## Verification gates

Each coder runs only the scoped commands in its chunk. No `just test`, `just
ci`, full-workspace compile, e2e, migration, or live-state binary invocation
is part of this handoff. If a binary invocation is needed for a focused check,
set `XDG_STATE_HOME` to a fresh temporary directory first. Required focused
checks include core editor/config/target unit tests, control schema snapshot
test, catalog/surface/completion ratchets, and host action/help tests.
