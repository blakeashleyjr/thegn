# Tasks

## 1. Core data + parsers + CLI seams (`thegn-core::github`)

- [x] 1.1 Add `PrComment`, `PrReview`, `ReviewThread`, `PrConversation`,
      `PrDiff`/`DiffFile`/`DiffHunk`/`DiffLine`/`DiffLineKind`, `ReviewState`.
- [x] 1.2 Add `headRefOid` to `PR_FIELDS` + `PrStatus.head_ref_oid`.
- [x] 1.3 Pure parsers `parse_unified_diff` + `parse_conversation` (unit-tested).
- [x] 1.4 Seam fns: `comment_pr`, `submit_review` (body required for
      request-changes/comment), `reply_to_thread` (GraphQL), `pr_diff`,
      `add_line_comment` (REST), `conversation` (GraphQL).

## 2. Service layer (`thegn-svc` `GhBackend`)

- [x] 2.1 Six new trait methods, default-impl'd to the core fns (CliGh free).
- [x] 2.2 `GhNative` inherits CLI for all six (writes CLI-only invariant);
      `headRefOid` threaded through the native `PR_QUERY` + parse.

## 3. Full-screen PR view (`thegn-host/src/pr_view.rs`)

- [x] 3.1 `PrView` / `PrTab` / `Composer`/`ComposerTarget` / `TextArea` /
      `PrViewOutcome` / `PrViewAction` / `PrViewData`.
- [x] 3.2 Overview + Checks tabs from the cached `PrStatus`/checks.
- [x] 3.3 Navigation (tab switch, cursor, scroll-follows-selection), rendering
      (layer + seg), footer hints.
- [x] 3.4 Host unit tests: tab nav, checks open/rerun, composer flow, files
      expand + line-comment anchoring, wrap.

## 4. Async reads

- [x] 4.1 `hydrate::spawn_pr_view_fetch` (diff + conversation off-loop) +
      `pr_view_tx` channel + generation single-flight.
- [x] 4.2 Conversation + Files tabs render the loaded data.

## 5. Composer + writes

- [x] 5.1 Composer sub-mode (Ctrl-D submit, Enter newline, Esc cancel, paste).
- [x] 5.2 `actions::run_pr_view_action` routes each action to `gh` off-loop.
- [x] 5.3 `RefreshKind::Pr` re-fetches the open view after a write.

## 6. CLI parity + wiring + validation

- [x] 6.1 `thegn pr comment|review|diff` subcommands.
- [x] 6.2 `run.rs` wiring: slot, channel drain, dispatch, render, open-on-Enter,
      paste routing.
- [x] 6.3 `test/smoke.sh` coverage for `pr comment|review|diff` seams.
- [x] 6.4 Green: fmt-check, clippy (core/svc/host), core+svc+host unit tests,
      openspec-validate --strict, smoke (`pr comment|review|diff` parse), god-file
      ratchet (run.rs shrank 22139→22051). Live GitHub round-trip still pending.
