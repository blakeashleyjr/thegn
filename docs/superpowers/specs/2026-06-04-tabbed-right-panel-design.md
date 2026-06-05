# Tabbed Right Panel: Modified Files + PR + Checks

**Date:** 2026-06-04
**Status:** Approved
**Build Phase:** P1 — Foundation

## Objective

Transform the single-view right panel (`plugin/panel/src/main.rs`) into a
three-tab WASM plugin — Modified Files (drill-down diff), PR status/actions,
and CI Checks — with keyboard-driven navigation, in-panel file diff viewing,
and editor integration.

## Tab Model

```
┌──────────────────────────────────────┐
│  DIFF │ PR │ CHECKS                  │  ← tab bar
│──────────────────────────────────────│
│  [body: context-dependent]           │
│──────────────────────────────────────│
│  [context-sensitive help bar]        │
└──────────────────────────────────────┘
```

### Tab 1 — Modified Files

Two views in a stack:

- **FileList** (default): scrollable cursor-navigated list of `git diff
--name-status` entries. Cursor highlight via selection bg. `Enter` drills
  into a single-file full diff. `o` opens the file in `$EDITOR` floating pane.
- **FileDiff**: full colorized diff of the selected file, half-page scrollable.
  `Esc` returns to FileList. `o` opens in editor.

### Tab 2 — PR

Retains all current PR rendering + actions:
`o` (open in browser), `c` (create), `m` (merge), `a` (approve), `r` (rerun).

### Tab 3 — Checks

Detailed CI check list from `statusCheckRollup` — each check's name and
conclusion with color coding. `r` re-runs failed checks.

## State Structure

```rust
struct State {
    // existing: session, active_tab, identity, worktree, pr, hidden, focused, my_id
    current_tab: Tab,
    diff_view: DiffView,
    diff_scroll: usize,      // half-page scroll offset for FileDiff
    files: Vec<FileEntry>,   // TSV from `diff --files`
    file_diff: String,       // raw output from `diff --file <path>`
    status_line: String,
}

enum Tab { Diff, Pr, Checks }

enum DiffView { FileList, FileDiff }

struct FileEntry {
    status: char,  // M, A, D, R, C, ?
    path: String,
}
```

## Key Bindings

| Key         | Diff:FileList | Diff:FileDiff | PR              | Checks       |
| ----------- | ------------- | ------------- | --------------- | ------------ |
| `1`/`2`/`3` | Switch tab    | Switch tab    | Switch tab      | Switch tab   |
| `Tab`       | Cycle tab     | Cycle tab     | Cycle tab       | Cycle tab    |
| `j`/`↓`     | Cursor down   | Scroll ½-page | —               | —            |
| `k`/`↑`     | Cursor up     | Scroll ½-page | —               | —            |
| `Enter`     | Drill to diff | —             | —               | —            |
| `Esc`       | —             | Back to list  | —               | —            |
| `o`         | Edit file     | Edit file     | Open PR browser | —            |
| `c`         | —             | —             | Create PR       | —            |
| `m`         | —             | —             | Merge PR        | —            |
| `a`         | —             | —             | Approve PR      | —            |
| `r`         | —             | —             | Rerun checks    | Rerun checks |
| `f`         | Refresh       | Refresh       | Refresh         | Refresh      |

## New CLI Commands

Add to `src/commands/diff.rs` and `src/cli.rs`:

```
superzej diff --files --worktree <wt>
  → TSV output: <status>\t<path>\n
  → Based on merge-base target

superzej diff --file <path> --worktree <wt>
  → Colorized git diff -- <path>
  → Piped through delta when available
```

## Data Flow

```
TabUpdate / Timer
  ↓
resolve-worktree  ──→  worktree path
  ↓ (parallel)
pr status --json  ──→  pr (Value)
diff --files       ──→  files (Vec<FileEntry>)
  ↓
render(current_tab, files, pr)
  ↓
Enter on FileList → diff --file <path>  ──→  file_diff (String)
  → DiffView = FileDiff
```

## Render Layout

**Tab bar (row 0):** Three segments — `DIFF` / `PR` / `CHECKS`. Active tab
gets cyan-bg + dark-fg (focused) or bright-fg (unfocused). Inactive = muted.

**Body (rows 2..N-2):** context-dependent.

**Help bar (row N-1):** Dimmed context-sensitive key hints.

## Implementation Phases

| Phase  | Scope                                                                  | Est. Δ |
| ------ | ---------------------------------------------------------------------- | ------ |
| **P1** | Tab enum, tab bar render, `1`/`2`/`3` + Tab switching, PR/Checks stubs | ~80    |
| **P2** | `--files` and `--file` in `diff.rs` + `cli.rs`                         | ~40    |
| **P3** | FileList render + cursor nav + Enter drill-in                          | ~100   |
| **P4** | FileDiff half-page scroll + Esc back + o edits                         | ~70    |
| **P5** | Port PR render into PR tab                                             | ~60    |
| **P6** | Checks list from statusCheckRollup                                     | ~50    |
| **P7** | Timer refresh for files + pr (no file_diff re-fetch)                   | ~30    |

## Edge Cases

- **Deleted file:** diff shows removal; `o` does nothing.
- **Untracked file:** no git diff — show "untracked (no diff)" placeholder;
  `o` still opens for editing.
- **No worktree:** "focus a worktree tab" in all tabs.
- **Large file lists:** natural scroll via offset; cursor wraps.
- **No new dependencies:** `serde_json` already present in plugin.
- **`o` key is context-dependent:** Diff tab = edit file; PR tab = open PR.
  Help bar clarifies.
