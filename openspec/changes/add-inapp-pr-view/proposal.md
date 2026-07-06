# Add a full in-app PR workflow view

## Summary

Selecting a PR in the panel today does one thing: `xdg-open`s it in the browser.
This change makes the full pull-request workflow happen inside `szhost`. The
panel's `Section::Pr` stays the at-a-glance summary, but pressing **Enter** now
opens a new near-full-screen **PR view** modal (`crates/superzej-host/src/pr_view.rs`)
with four tabs — **Overview**, **Checks**, **Conversation**, **Files** — that
lets the user read CI checks, the comment/review timeline, review threads, and
the unified diff, and act on all of it without leaving the app: merge, approve,
request-changes/comment reviews (with a body), PR-level comments, thread replies,
re-run failed checks, and inline line-level review comments. `o` still opens the
browser as an escape hatch.

The service layer gains the missing write and read seams (in
`superzej-core::github` and the `GhBackend` trait): post comment, submit review
with state, reply to a thread, fetch and parse the unified diff, post an inline
line comment, and a one-round-trip conversation fetch (comments + reviews +
threads). Every seam has CLI parity (`superzej pr comment|review|diff`), so the
capability exists headless too. Diff and conversation load off the event loop;
every write runs off-thread and pulses a `RefreshKind::Pr` that re-hydrates the
panel cache and, when the view is open, re-fetches its data so a just-posted
comment shows up.

## Impact

- **Panel capability** — the `PR` section becomes a launch point for a full
  workflow surface, not just a browser shortcut.
- **GitHub seam (`superzej-svc` GitBackend / `superzej-core::github`)** — six new
  operations (comment, submit_review, reply_thread, pr_diff, add_line_comment,
  conversation). Reads may go native (octocrab) later; **writes stay CLI-only**
  (delegate to `gh`), preserving the "every native path falls back to CLI"
  invariant. Adds `headRefOid` to the PR fetch (the `commit_id` an inline
  comment anchors to).
- **State DB** — none. The existing `pr_cache` carries the extra `headRefOid`/
  `mergeable` fields via serde defaults; no migration.
- **Event loop** — one new `Option<PrView>` modal slot + one async data channel
  (`pr_view_tx`), dispatched/rendered beside the existing detail overlay. All
  new host logic lives in `pr_view.rs` / `actions.rs` / `hydrate.rs`; `run.rs`
  gets only the slot, one dispatch block, one render call, the channel drain,
  and the open-on-Enter site (the god-file ratchet stays green).
- No AI-layer dependency; this is AI-free shell surface.

## Rationale

superzej is a git-worktree IDE whose whole point is staying in one terminal
surface. PR review is the one workflow that still ejected the user to a browser,
breaking that promise. The data seam was already half-built — `pr_status`,
`review_threads`, `reviews`, `approve`, `merge`, `rerun_failed` exist — so the
gap was the write verbs (comment/review/reply/line-comment), the diff, and a
place to show it all. Reusing the established modal patterns (`DetailOverlay`
lifecycle, `layer`/`seg` rendering, `spawn_pr_action` off-loop writes, the
generation-guarded async fetch) keeps the addition small and idiomatic.

## Non-goals

- **No new GitHub write surface beyond review flow** — no label/assignee/
  milestone/reviewer-request editing, no title/body edit, no merge-conflict
  resolution UI. Those can follow.
- **No native (octocrab) write path** — writes delegate to `gh`; only the
  conversation/diff reads are candidates for a later native optimization.
- **No syntax highlighting in the inline diff** — colored add/del/context is
  enough for v1 (the panel's syntect preview path can be adopted later).
- **No rich text editor** — the composer is append + backspace + newline +
  paste; mid-string editing can come later.
