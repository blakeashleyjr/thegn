# Show PR review comments in the diff, and hand one to the agent

Linear: THE-27

## Why

The in-app PR view fetches everything a review thread is — path, line, diff
hunk, every comment, resolved state (`thegn_core::forge::model::ReviewThread`)
— and then renders it only in the **Conversation** tab. The **Files** tab
renders the same diff those threads anchor to with no trace of them
(`pr_view.rs::files_body` walks hunks and lines only). So the reviewer's core
loop is exactly the complaint that seeded this issue (orca#4693): read a
comment in one tab, switch tabs, find the file, find the line, parse the
change, switch back. VS Code's GitHub PR extension made "comments live in the
diff" table stakes.

The second half is the handoff: a review comment usually _is_ a task, and
thegn already has both dispatch shapes — a live agent pane the worktree
remembers (`agent` capability: "The worktree remembers its agent") and the
headless `agent_task` engine whose `PrReview` kind already formats unresolved
threads into a prompt. What is missing is the one keypress from a thread row
to either of them (roadmap T 262 "Inline comments → follow-up prompt",
Q 216 "Follow-up prompt into live agent").

## What Changes

- **Inline threads in the PR view Files tab.** Expanding a file interleaves
  its review threads directly under their anchor lines (matching
  `ReviewThread.path` + `line` against `DiffLine.new_lineno`). Unresolved
  threads render by default; resolved ones behind a toggle. Threads whose
  anchor line is not present in the current diff ("outdated") collect in a
  block at the end of the file rather than being dropped. Thread rows are
  selectable, and the existing reply composer opens from them exactly as it
  does in the Conversation tab.
- **Unresolved-thread markers on file rows.** The Files tab's file list and
  the panel `Pr` section's thread summary gain a per-file unresolved count
  (glyph via the caps chokepoint, no literal at the draw site), so "which
  files still have feedback" is visible before drilling in.
- **Pass a thread to the agent.** A key on any thread row (Files or
  Conversation tab) hands that thread to an agent:
  - **Live pane** (default when the worktree's remembered agent pane is
    running): the thread is formatted as a follow-up prompt — file:line, the
    anchoring diff hunk, every comment — and inserted into that pane's input
    via bracketed paste, sanitized, **without** a trailing newline: the user
    reviews and submits.
  - **Headless** (explicit variant, or fallback offer when no pane is
    running): a single-thread `PrReview` dispatch through the shared
    `agent_task` engine + `agent_run`, inheriting the PR family's rules
    (push with `--force-with-lease`, never merge/approve/resolve).
- Both handoffs are additive: with no agent configured and no agent pane
  running, the key reports why in `model.status` and does nothing.

## Impact

- Roadmap: **T 262** (inline comments → follow-up prompt) is this change;
  **Q 216** (follow-up prompt into live agent) gets its first producer;
  builds directly on **Z 333** (PR review comments, shipped in
  `add-inapp-pr-view`) and **T 260** (diff review pane).
- Specs: `panel` — MODIFIED "Full in-app PR workflow view" (inline threads +
  handoff), ADDED thread-marker and handoff requirements.
- Code: `crates/thegn-host/src/pr_view.rs` (thread interleave, markers,
  handoff key), a small pure anchor-matching module in `thegn-core`
  (line-anchor → diff-row mapping + prompt formatting, unit-tested under the
  95% gate), `crates/thegn-host/src/pane_writer.rs` (sanitized paste path),
  dispatch via existing `thegn_core::agent_task` / `agent_run`.
- Help: `docs/help/review-a-pr.md` (context `panel:pr`) documents the new
  keys; the help prose ratchet requires the chords to be mentioned.
- **No DB schema change.** No new config table (one `[forge]`/`[panel]`-level
  key at most for the resolved-threads default; specced in design).
- In-flight overlap: `add-pr-review-viewed-stacked` also edits the Files tab
  (viewed glyphs, per-commit walker) — same file, different rows; the thread
  interleave must key off the rendered diff range so it composes with the
  stacked walker. `add-pr-queue` (implemented) supplies the headless
  `PrReview` machinery this reuses; `add-agent-task-engine` supplies the
  engine. THE-22's `add-watched-pr-comment-tasks` automates what this change
  does manually — they share the single-thread prompt formatter.

## Non-goals

- **Anchoring PR threads onto the local (uncommitted) changes view.** The
  panel `Changes` section diffs the working tree, where PR-head line anchors
  drift; mapping them honestly is its own feature. This change marks and
  anchors threads only where the rendered diff _is_ the PR diff.
- **Resolving threads.** Reply-never-resolve stands (the pr-queue team-safety
  rule); resolution stays the reviewer's call.
- **Auto-submitting the pasted prompt.** The live-pane handoff inserts text;
  the human presses Enter. Autonomous handling of threads is THE-22
  (`add-watched-pr-comment-tasks`), not a side effect of a paste.
- **New forge fetching.** Threads, hunks, and line anchors all already ride
  on `PrConversation`/`PrDiff`; this is a rendering + dispatch change.
