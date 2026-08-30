# THE-27 architecture design

## Outcome

Add one review-feedback snapshot per worktree, fetched through the forge seam
off the compositor loop and stored as a best-effort SQLite cache. The snapshot
contains the PR diff, top-level comments/reviews, and full review threads. A
pure `thegn-core` projection anchors thread rows to the rendered new-side diff
and renders bounded, delimited handoff text. The host then uses that projection
in both the PR Files tab and the right-panel full-screen diff/changes surface.

The existing local worktree diff remains the default view. When a PR snapshot is
available, the full-screen diff view has an explicit `PR review` mode whose rows
are the fetched PR diff, so a comment is never presented as anchored to a
different uncommitted patch. The PR panel continues to show review status and
adds top-level feedback plus per-file unresolved counts. This is the honest
way to satisfy the issue without changing the meaning of the current local
Changes view.

THE-22 is deliberately not implemented: this change is a human-triggered
handoff only. Its data and the pure formatter are reusable by the watched-PR
task later.

## Verified current design and draft delta

The openspec draft is useful but stale in two binding places:

- `openspec/changes/add-pr-comments-in-diff/proposal.md:27-51` and
  `design.md:26-48` correctly describe inline rows, outdated buckets, a
  resolved toggle, and handoff shapes.
- `crates/thegn-host/src/pr_view.rs:120-207` already has the deep conversation
  model, PR tabs, thread-reply target, generation guard, and action enum; its
  Files implementation still walks only diff lines (`pr_view.rs:297-304`), so
  inline rows are not landed.
- `crates/thegn-core/src/forge/model.rs:494-553` already carries full threads,
  conversations, and line-numbered diff rows. The parser is already pure and
  tested (`model.rs:687-765`); anchoring and prompt projection are absent.
- `crates/thegn-core/src/forge/mod.rs:285-314` already provides the optional
  `review_threads`, `conversation`, and `pr_diff` operations, while
  `thegn-svc/src/forge/mod.rs:59-130` forwards them through the ladder. The
  GitHub CLI implementation is isolated in `thegn-core/src/github.rs:578-655`
  and the native implementation advertises only the operations it owns
  (`thegn-svc/src/forge/native.rs:383-408`). Do not add a vendor call in the
  host.
- `crates/thegn-host/src/actions.rs:704-747` already fetches conversation and
  PR diff off-loop and pulses the waker, and `hydrate.rs:3482-3591` already
  demonstrates definitive-state cache writes that preserve stale data during a
  transient outage. These are the patterns to extend, not duplicate.
- `crates/thegn-core/src/agent_task.rs:491-507` already defines the PR-review
  prompt and its rules, `pr_driver.rs:582-677` already maps `TaskKind::PrReview`
  to the `{threads}` variable, and `pane_writer.rs:182-205` plus
  `run.rs:235-240` already provide nonblocking bracketed paste.

The draft’s exclusions are cut:

- `proposal.md:68-70` says “no DB schema change”; this conflicts with the
  issue’s cache requirement and with the deep view needing data after the
  compact `PrPanel` cache. Additive schema v62 is required.
- `proposal.md:80-83` and `design.md:110-115` exclude the Changes surface. The
  design here preserves local diff semantics but adds an explicit PR-review
  source in the same full-screen diff surface.
- `proposal.md:89-90` and `design.md:78-80` say no new fetching. The existing
  fetch is off-loop, but it is not cached and is not available to the diff
  modal; extend one fetch/cache pipeline rather than assuming the current
  transient modal feed is sufficient.
- The draft’s `g/G` handoff (`design.md:50-70`) conflicts with existing `g/G`
  first/last navigation in `pr_view.rs:368-372` and `diff_view.rs:206-214`.
  Use `n/N` for next/previous thread, `p/P` for pass selected/all unresolved,
  and `Enter` on a thread for jump-to-anchor. Existing `r` remains reply.

## Core data and seam

### Review snapshot

Add a substrate-free `PrReviewSnapshot` beside the forge models. It is a cache
wire value, not an additional source of truth:

```text
PrReviewSnapshot {
    worktree_key, branch, pr_number, head_oid, fetched_at,
    conversation: PrConversation,
    diff: PrDiff,
}
```

`PrConversation.comments` and non-empty `PrReview.body` are PR-level feedback;
`ReviewThread` values are the anchored or outdated feedback. Keep the thread
IDs and all comments for replies and prompts. Do not flatten the snapshot into
`ReviewThreadRow`; that existing row is intentionally only the compact panel
projection (`forge/model.rs:85-97`). Add a serde default so a future cache
extension can read old payloads.

Add `review.rs` (or an equivalently small sibling module) with pure functions:

- `anchor_threads(&PrDiff, &[ReviewThread]) -> AnchoredReview`: exact
  `(path, new_lineno)` matches produce a file/line row anchor; `line == None`,
  a deleted-side anchor, a renamed-path miss, or an absent line goes into that
  file’s `outdated` bucket or the general bucket. Never choose the nearest line.
- `visible_threads` filters unresolved by default and includes resolved only
  when the view-local toggle is true; stable ordering is file/diff order, then
  source order, with unresolved first only where the existing compact summary
  promises that order.
- `format_review_feedback` emits bounded text for one selected thread or all
  unresolved threads. Include PR identity, `path:line` when present, diff hunk,
  every thread comment with author, and PR-level comments only in the all-
  feedback context. Mark remote bodies as data in delimiters and strip C0/C1
  controls except newline/tab. It must return text without a final newline for
  live paste. The existing core `agent_task::render_prompt` remains the only
  template renderer; pass its `{threads}` value through this function.

Unit-test exact hits, misses, deleted anchors, multiple threads on one line,
resolved filtering, top-level comments, caps, delimiters, control stripping,
and no-final-newline. Keep this module free of termwiz, tokio, SQLite, forge
clients, and host types; it is part of the core coverage surface.

The forge seam does not gain a vendor-specific method. `review_threads` remains
the optional operation and `conversation` is the deep operation; this capability
already exists in `ForgeCaps`/`Forge` (`forge/mod.rs:105-126,285-314`) and is
forwarded by the service ladder (`thegn-svc/src/forge/mod.rs:59-130`). The
native GitHub layer correctly leaves those caps false while the CLI fallback
provides them. That is the reserved-op behavior required here and is already
satisfied on this branch. For a future forge kind that cannot provide them,
keep caps false and report the kind through the existing `ProbeReport::reserved`
path (`thegn-core/src/seam.rs:141-152`); do not return an empty successful
conversation, which would erase stale feedback. The existing Forgejo/Gitea
reserved kinds (`config_forge.rs:14-23`, factory
`thegn-svc/src/forge/mod.rs:305-315`) are the future extension point. Vendor
commands remain only in `github.rs`/forge implementation files.

## SQLite cache and fetch lifecycle

Add `pr_review_cache` as an additive table in schema v62, keyed by the canonical
worktree cache key, with branch, PR number, head OID, JSON snapshot, and
`fetched_at`. Extend the `CacheStore` trait (`store/cache.rs:14-24`) and its
SQLite implementation (`db_cache.rs:18-52`) with typed get/put methods. Keep
the payload atomic: a partial conversation/diff result must not replace the
last complete snapshot.

Update `db.rs`’s schema version and verifier (`db.rs:131-136`, `db.rs:932-935`)
and `db_migrate.rs`’s idempotent additive ladder/verifier. The migration must
create only the new cache table, preserve all old rows, and have a pre-v61/pre-
v62 migration test plus an idempotence test. The cache is not live state: a
read failure is a miss, a transient forge error leaves the old snapshot intact,
and a definitive empty/no-PR answer may replace it. Validate branch, PR number,
and head OID before presenting a cached snapshot; a mismatch is stale and must
be labeled or ignored, never silently attached to the current PR.

Extend the existing off-loop PR refresh. On a PR with a usable repo reference,
fetch `conversation` and `pr_diff` through `forge_handle::get().for_loc(&loc)`;
write one complete snapshot only when both required reads are valid, then pulse
the `TerminalWaker`. Deliver a generation-tagged snapshot to the modal and
rehydrate the panel from cache. Cache reads/fetches/writes remain outside the
event loop; the loop only drains a channel and marks the frame dirty. Unsupported
or unauthenticated providers degrade to the current compact summary/loading
state and show a capability note; they never crash or fabricate threads.

## Host presentation and navigation

Create a small host row/projection module if needed rather than growing either
god file. Both `PrView` and `DiffView` consume the same `AnchoredReview` plus
the existing `PrDiff`/`DiffLine` renderer. No second diff parser or alternate
line-number model is allowed.

### PR view

In `pr_view.rs`, expanded Files rows interleave a selectable thread header and
wrapped comment rows immediately after the matching `DiffLine`. Outdated rows
appear in an explicit end-of-file feedback block. File-list rows show the
unresolved count. The Conversation tab shows top-level comments/reviews and
all thread rows; the selected thread identity is shared with Files.

Actions:

- `n`/`N`: next/previous thread in the current PR, unresolved first; update
  tab/file/row and scroll to the anchor when it exists.
- `Enter`: jump the selected thread to its file and line. An outdated or
  top-level item stays in Conversation and reports “no diff anchor”.
- `r`: existing thread reply composer.
- `p`: pass selected thread; `P`: pass all unresolved threads.
- resolved toggle is view-local and has a visible key hint; it is not a config
  key.

Use the existing `Tok`/theme slots, `caps::active_glyphs()`, and shared diff
line functions. Add no glyph/color literals at draw sites; the existing
literal ratchets already pin `pr_view.rs` and `diff_view.rs`
(`test/glyph-literal-ratchet.txt:35`, `test/color-literal-ratchet.txt:19`).

### Right-panel Changes/diff surface

Do not reinterpret the existing local `ChangeRow`/staging diff. Add a PR review
source to the full-screen `DiffView` data and a source toggle (use an unused
view-local key such as `Tab`, not `t`, which already toggles structural mode).
`Worktree` remains the default and continues to include uncommitted changes;
`PR review` renders the cached/fetched `PrDiff`, interleaves the same anchored
thread rows, and includes a top-level feedback block. The footer names the
source and says when the PR snapshot is stale, loading, unsupported, or absent.
The panel `Pr` block shows the top-level feedback count and unresolved per-file
counts, and its existing `Enter` still opens the PR modal. This makes both the
right-panel diff/changes workflow and the PR panel discoverable without lying
about local line anchors.

## Handoff and interlock

Add a host-only `ReviewHandoff` action/result path, preferably in a new
`review_handoff.rs`, so `run.rs` and `actions.rs` do not accumulate prompt,
pane-selection, and dispatch policy. The action receives the active worktree,
PR facts, snapshot, and selected/all-unresolved selection.

1. Resolve a live agent pane only within the active worktree’s own session tabs:
   inspect the pane’s actual foreground program (`PtyPane::foreground_program`,
   `pane.rs:537-566`) against the configured agent-program predicate. Do not
   use the focused pane as a proxy and never cross worktree groups.
2. For a live pane, call the existing `paste_text_into_pane`/pane-writer path
   (`run.rs:235-240`) with the core-rendered text, bracketed paste when the
   emulator requests it, no trailing newline, and focus that pane. A full/dead
   writer is a status/toast result, not an event-loop block.
3. If no live pane exists, use the existing PR-review `TaskKind` and configured
   agent template, with the one-thread or all-unresolved `{threads}` value from
   core. Preserve the existing PR rules: push with force-with-lease, never
   merge/approve/resolve. Reuse `agent_run` off-loop and pulse the waker on
   completion; no new TaskKind and no new config key.
4. If neither a pane nor a configured headless agent exists, report the reason
   and make no write. Do not auto-submit pasted text.

The public `sessions.input` control route and MCP `--allow-session-input`
interlock (`crates/thegn-host/src/cmd/mcp.rs:380-401`, route catalog in
`crates/thegn-svc/src/control/routes.rs:39-44,152-153`) remain unchanged. The
TUI’s own pane writer is already the authorized host session-input path; do not
invent an internal HTTP call that bypasses the flag. If an implementation
chooses to cross the public control seam instead, it must add the catalog/wire/
scope/snapshot changes and honor the same allow-session-input policy in that
same chunk; the recommended design does not need to.

## Help, ratchets, and validation

Update `docs/help/review-a-pr.md` with the PR review source toggle, `n/N`,
`Enter` jump, `p/P` handoff, resolved toggle, stale/unsupported behavior, and
the no-auto-submit rule. These are view-internal keys, so they do not become
`ACTION_SPECS`; the authored page still satisfies the help prose/context
ratchets. Remove only any now-covered allowlist lines; never add new debt.

No new config key means no `config/config.toml.example` or env-overlay change.
No control verb means no completion-slot or control-schema change. The coder
must still run the ratchet checks and update any affected ratchet in the same
commit if implementation reveals a new catalog/config/action surface. The
existing glyph/color ratchets must remain satisfied.

Expected snapshot impact is listed for later implementation, but this design
commit must not re-record e2e output:

- the PR section of `test/muse/snapshots/panel_work__work/xterm__100x30__linux.txt`;
- the Changes/diff panel fixtures under `test/muse/snapshots/` at 100x30 and
  160x40;
- new PR Files and DiffView modal fixtures, if the repository’s e2e harness
  materializes them.

Do not run e2e or `just test`/`just ci` in the coder chunks. Never invoke the
binary or a migration against the worktree’s live state DB; any manual binary
probe must set `XDG_STATE_HOME` to a newly-created temporary directory.
