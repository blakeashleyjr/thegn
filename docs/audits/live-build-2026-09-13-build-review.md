**Build, queue, database and worktree review — September 13, 2026**

This review covers findings 4, 9 and 11 of the [initial live-build audit](live-build-2026-09-13.md), plus build identity and related cleanup authority. It inspects current main `f4c1355b` and the current working-tree recipe. Runtime reads were read-only; isolated build fixtures used `/tmp`. No production code, live configuration, queue state, worktrees, processes or external issues were changed.

**B1 — Correct the current-state sccache claim; keep the upgrade concern.**

The current working tree contains the intended `RUSTC_WRAPPER='' cargo build --release --features profiling -p thegn-host` override at [justfile:349](../../justfile). It is an uncommitted modification. The earlier audit's observation that the override was absent describes its captured tree; it is **not the current state**. No cause for the intervening change is inferred. The modification was preserved during review.

The separate upgrade concern is confirmed: `live` still builds into the shared release target, uses two broad `pkill -f` matches, sets `THEGN_DATABASE_MIGRATION_EXECUTABLE`, and truncates the stderr capture. There is no staged executable installation or online database backup in that recipe. [THE-613](https://linear.app/blakeashley/issue/THE-613) remains In Progress, and `fix/live-upgrade-safety` is not merged into main. Source availability on that branch is not proof that the user's installed launcher has its protection. This review does not certify that branch's full implementation.

Minimal follow-up: preserve the cache override in the eventual reviewed change; finish the staged upgrade separately, with exact process ownership, backup and failure-path verification. Do not equate a running replacement host with successful pane/session restoration.

**B2 — High: unadvanced fold results can still be persisted and acted upon as landed. Confirmed source defect.**

[integrate.rs:971–1017](../../crates/thegn-host/src/integrate.rs) populates `FoldReport.landed` from a speculative fold plan while initializing `advanced=false`. Gate-error and gate-failure returns preserve that list. Only a successful target compare-and-swap later sets `advanced=true`.

Nevertheless [integrate::persist, lines 586–626](../../crates/thegn-host/src/integrate.rs) enqueues entries, writes status `landed`, and applies the `Landed` lifecycle from that list without checking advancement. [cmd/integrate.rs:145–217](../../crates/thegn-host/src/cmd/integrate.rs) calls persistence and expiry sweeping before reporting the gate outcome, prints individual landed lines from the speculative list, and ultimately returns `Ok(())` even for a failed gate. The UI also treats a nonempty landed list as successful integration in [handlers/merge_queue.rs:270–296](../../crates/thegn-host/src/handlers/merge_queue.rs).

This strengthens the initial “blocked queue item” finding: the defect is not merely stale administrative state. It remains an incorrect outcome/publication boundary that can authorize cleanup of unlanded work under destructive lifecycle settings. The saved protective correction for the delivery-inventory row and [THE-589](https://linear.app/blakeashley/issue/THE-589) document a prior reproduction. The repair branch is not merged.

Minimal follow-up: distinguish speculative candidates from committed outcomes, require successful target advancement or separately established canonical ancestry before publishing landed status, and make persistence/lifecycle/reporting/CLI exit status obey the same result. Tests must cover red gate, infrastructure error, failed compare-and-swap, successful advancement and disabled automatic landing. A guard only on CLI wording is insufficient.

**B3 — High: automatic merged-worktree sweeping applies one repository's policy to global rows. Confirmed additional source defect.**

[merge_sweep.rs:40–50](../../crates/thegn-host/src/merge_sweep.rs) obtains all database `landed` rows without repository filtering. `sweep(cfg, repo_root, force)` then applies the supplied repository's expiry policy to that global set and passes the same `repo_root` to every cleanup at lines 61–94.

This is the open [THE-237](https://linear.app/blakeashley/issue/THE-237) issue, not a newly discovered incident. The observed database contains rows for multiple repositories, so the cross-repository selection is relevant to the user's state; this review did not invoke cleanup or demonstrate damage.

Further outcome problems in the same path: `sweep` appends an entry to `collected` after calling a void-returning cleanup function, even if cleanup was refused. [merge_lifecycle.rs:207–260](../../crates/thegn-host/src/merge_lifecycle.rs) removes queue entries on several refusal paths, including another active destroy claim, detected dirtiness and failed physical removal. Thus “collected” can be inaccurate, and retry evidence can disappear.

The initial audit correctly treated the logged refusals as reasons not to force deletion. It should not be read as a certification of the whole cleanup transaction. Missing or unreadable directories are also treated as not dirty at `merge_lifecycle.rs:189`; safety cannot rest on that observation alone.

Minimal follow-up: select candidates by verified owning-repository identity before policy or side effects, revalidate exact ownership, return a typed cleanup outcome, and retain queue evidence when cleanup is refused. Use private two-repository fixtures with different policies, dirty/unreadable/missing paths and a failed cleanup. Do not run a real sweep as a verification step.

**B4 — The current queue's landed records have Git-history support; the blocked row remains blocked.**

A read-only query at approximately 19:40 found the same **34** queue rows: 33 `landed`, one `gate_error`. For 29 landed rows with stored result OIDs, each result is an ancestor of its owning repository's configured target. Four other landed rows have no result OID; each current named branch tip is also an ancestor of that target. The relevant repositories were nonshallow and had no replacement refs or graft file; ancestry was additionally checked with replacement objects disabled and the graft override removed from the audit subprocess environment.

This does **not** reconstruct the historical gate verdict or prove correct inclusion under every merge strategy. It does mean this read did not find another currently marked-landed commit outside target history. The one gate-error row, `fix/delivery-inventory-severity`, remains intentionally held. Its branch is not in main. The source defect in B2 remains even though the previously affected row was protectively corrected.

Captured evidence: `/tmp/thegn-live-review-20260913/queue-ancestry.json` and `history-preconditions.json`. No database writes or repair commands were used. The initial SQLite integrity results remain valid for their snapshot; this follow-up is a semantic history check, not a second full database integrity scan.

**B5 — Medium: the build-script watch defect is reproduced in actual isolated Cargo builds.**

[build.rs:45–48](../../crates/thegn-host/build.rs) watches `src`, `build.rs` and `../../.git/HEAD`. The latter is not the actual HEAD path in a linked worktree, and a main-checkout branch-tip update changes its ref without necessarily modifying `.git/HEAD`.

A tiny dependency-free Cargo fixture used the **unchanged actual host build script**, under a new private Git repository in `/tmp`. It reproduced both cases:

- Main checkout: initial commit `38d56ab` was embedded correctly. An empty commit advanced HEAD to `7673520` without changing source or `.git/HEAD`. The next Cargo build reported `Fresh`, and the binary still embedded **`38d56ab`**.
- Linked worktree: immediately repeating an unchanged build produced Cargo's diagnostic **“Dirty … the file `../../.git/HEAD` is missing.”**

The main-checkout proof shows stale identity when only Git metadata changes; it does not establish stale executable behavior when source inputs actually change. The linked-worktree proof directly explains unnecessary build-script reruns in that case. Neither result attributes every slow build to this defect.

This corroborates and expands In Progress [THE-575](https://linear.app/blakeashley/issue/THE-575). Fixture commands, compiler logs and `result.json` are retained under `/tmp/thegn-live-review-20260913/build-metadata-30qzlvsh/`. Four tiny Cargo builds were run; no application rebuild occurred.

Minimal follow-up: resolve worktree/common Git metadata, watch the relevant HEAD and ref/packed-ref inputs, handle detached/source-archive builds, and verify an unchanged linked build stays fresh while an actual commit change updates identity. Keep this separate from shared-target provenance guarantees.

**B6 — The delivery failure and stale-worktree observations stand, with bounded claims.**

`python3 scripts/delivery_state.py validate` still exits 1 for missing mapping of `preserve-config-validation-severity`. [THE-586](https://linear.app/blakeashley/issue/THE-586) is still In Progress. The original 15 independent source checks are not a substitute for that failed aggregate gate.

[hydrate.rs:1518–1546 and 1582–1601](../../crates/thegn-host/src/hydrate.rs) deliberately retains missing/unreadable worktree directories while Git still lists them, and retains them if the Git query itself fails. This supports the initial caution about treating `prunable` or a sandbox's missing `/tmp` directory as proof of deletion. The actual known live records are three missing temporary paths; the larger audit-filesystem inventory needs an owning-host check. Ordinary retention and repeated notices are separate from B3's unsafe cleanup authority.

No stale worktree was pruned, no unmerged branch was removed, and no existing issue status was changed. Review priorities are B2/B3 before automatic cleanup or queue retries, B1 before the next live upgrade, and B5 for build reliability and diagnostic identity. The small delivery metadata edit remains necessary, but landing it must not bypass the failed-fold protections.
