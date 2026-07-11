# Design

## Where each piece lives (respecting the 3-crate split + god-file ratchet)

- **`thegn-core::github`** — pure serde structs + two pure parsers
  (`parse_unified_diff`, `parse_conversation`, both unit-tested) and the `gh`
  subprocess seams. The whole module is in the coverage `cov_ignore` list
  (subprocess), so it carries tests for the parsers but no coverage gate.
- **`thegn-svc` `GhBackend`** — six new async methods with default impls that
  delegate to the core `gh` fns, so `CliGh` gets them for free and `GhNative`
  inherits CLI behavior. This keeps the "every native path falls back to CLI"
  invariant with zero write-side octocrab surface.
- **`thegn-host/src/pr_view.rs`** — a NEW module (not an extension of
  `DetailOverlay`, which snapshots at open and takes no async data). Owns the
  view state, tabbed navigation, the composer sub-mode, and rendering. All the
  substantial host logic lives here.
- **`actions.rs`** — `run_pr_view_action`, mirroring `spawn_ci_action`: posts a
  status, runs the `gh` write on `spawn_blocking`, pulses `RefreshKind::Pr`.
- **`hydrate.rs`** — `spawn_pr_view_fetch` (diff + conversation off-loop),
  generation-single-flighted like `spawn_pr_cache_refresh`.
- **`run.rs`** — minimal: an `Option<PrView>` slot + a `pr_view_gen` counter, the
  `pr_view_tx` channel + its drain, one dispatch block (delegating to
  `actions::run_pr_view_action`), one render call beside `bar_detail`, the
  open-on-Enter site on `Section::Pr`, and paste routing. No PR logic in `run.rs`.

## Key decisions

- **Enter opens the view; `o` stays the browser.** Enter on the `PR` section used
  to open a review thread's file in an editor; the view subsumes that (Files +
  Conversation tabs), so Enter is repurposed. `o` (browser) is unchanged.
- **Data flow.** Overview + Checks render instantly from the panel's cached
  `PrStatus` (extended with `head_ref_oid`/`mergeable`, carried in `pr_cache` via
  serde defaults — no migration). Conversation + diff load async over
  `pr_view_tx`; stale generations are dropped. After any write, `RefreshKind::Pr`
  re-hydrates the panel cache and re-fetches the open view.
- **Inline-comment anchoring.** `parse_unified_diff` tracks per-line old/new line
  numbers; an inline comment anchors to the new-side (`RIGHT`) line and the PR
  head commit SHA (`head_ref_oid`), posted via `gh api POST …/pulls/{n}/comments`.
- **Thread replies** use the GraphQL `addPullRequestReviewThreadReply` mutation
  keyed by the review-thread node id (not a comment id); the conversation query
  selects `reviewThreads.nodes.id`.
- **Composer** is a small append/backspace/newline/paste `TextArea` (mid-string
  editing deferred). It routes by `ComposerTarget` to the correct API — PR
  comment vs review-with-state vs thread reply vs inline line comment — so the
  three distinct GitHub comment models never get conflated.

## Risks / mitigations

- **GraphQL correctness** — pure `parse_conversation` fixture test; reply/line
  comment start CLI-only so `gh api` does the heavy lifting.
- **Large diffs** — parsed once off-loop into `PrDiff`; the Files tab renders
  only the expanded file's hunks; scrolling never re-parses.
- **Rate limits** — the view fetch is generation-single-flighted; Overview/Checks
  reuse the cached `PrStatus`, so opening costs at most two extra reads; the
  existing circuit breaker + `PanelState` degrade cleanly.
- **run.rs ratchet** — enforced by keeping all logic in sibling modules.
