## Context

Phase 2 of the extensibility convergence. The forge is the seam the audit graded D: the host calls `thegn_core::github` free functions directly (40 sites), `GhBackend` is async-in-trait with one live method, `PrQueueForge` is the only correctly shaped piece. The `forge-leak` ratchet (phase 1) froze the debt; this change pays it.

## Goals / Non-Goals

**Goals:** one object-safe `Forge` trait every host site uses; GitHub native→CLI as a `Ladder`; `ForgeSet` routing by origin host; one error type that is also the seam error; one owner/repo parser; one auth probe; `pr_driver` testable with a fake; the ratchet's LEAK section empty.

**Non-Goals:** Forgejo/Gitea/GitLab implementations (kinds stay reserved); changing `GitHubIssuesBackend` (the issue seam's own GitHub impl stays, pinned IMPL); async-ifying the host.

## Decisions

- **Sync trait.** Every forge op is a subprocess (`gh`) or an octocrab call that the host already runs under a throwaway runtime on a blocking thread. A `BoxFuture` trait would force `Handle::block_on` bridges into `drive_queue` (sync, runs on CLI threads and `spawn_blocking`) — the hazard the inventory flagged. The provider-seams rule already allows "plain `&self` for blocking seams"; this change states the criterion explicitly: **a seam is sync when every implementation is process-bound or wraps its own async client**, async only when a native async client is the primary path _and_ callers are async. The native impl owns its `block_on` (a current-thread runtime built per call, as today).
- **Trait + models + CLI impl in core**, native + ladder + set in svc. Core has no tokio; the CLI impl is `gh` via `GitLoc::gh_command` (already in core's `cov_ignore`). This keeps the host's type imports (`PanelData` embeds `ReviewThreadRow`/`IssueRow`/`PrHeader`) on a core path and makes the trait usable from core tests.
- **`ForgeError` replaces `GhError`** (same variants + `Unsupported(&'static str)`), `impl SeamError`: NotInstalled→NotInstalled, NotAuthenticated→Auth, NoPr→NotFound, RateLimited→RateLimited, Offline→Transient, Other→Other. `describe()` moves onto the type. `PanelState` stays (it is the `pr_cache` wire/render shape) but is produced only by `PrPanel::from_result` — the `Result↔PanelState` round-trip in `prq.rs` disappears.
- **Op shape**: `pr_status(loc, number: Option<u64>) -> Result<PrStatus, ForgeError>` (number `None` = current branch) + `review_threads` + `issue_list` as separate ops; `pr_status_full`/`with_threads` become pure helpers on `ForgeExt`… no — keep it simple: a provided method `fn pr_panel(&self, loc, number, depth: PrDepth) -> PrPanel` on the trait composes them. `whoami(loc) -> Result<String>` replaces the three `gh auth status` probes and `pr_driver::viewer_login`. `parse_unified_diff` stays a free pure fn (it parses `git diff` too).
- **Ladder semantics**: `GithubNative` returns `Unsupported` for ops it doesn't implement and `NotConfigured` when no token / remote loc / open circuit → falls through to `GithubCli`; `Auth`/`NotFound`/`Transient` are final. This is exactly `ErrorClass::falls_through`.
- **`ForgeSet::for_loc`**: with no `[[forges]]` it returns the default ladder without spawning git; with entries it sniffs `origin` host once per call (callers are already on blocking threads). Reserved kinds produce no entry (registry reports them).
- **Host handle**: `forge_handle::{install, get}` over `OnceLock<Arc<ForgeSet>>`; `get()` without `install` builds from `Config::default()` so CLI subcommands and tests never panic. Installed once in `main` after config load (both the compositor and subcommand paths).
- **Connectivity**: the GhCircuit feed moves into `GithubNative`; `connectivity_gate::report_pr_panel` keeps classifying `PanelState` (it is still the host's view) — one producer per layer, no duplication.
- Render/event-loop: none. Help context: none (no new surface); `docs/help/review-a-pr.md` / `git-and-diffs.md` unchanged.

## Risks / Trade-offs

- [`PrPanel` serde must stay byte-compatible with cached rows] → the type moves unchanged; `pr_panel_round_trips_*` test moves with it and a `pr_cache` fixture test is added.
- [Native `pr_list` regression] → `GithubNative` keeps its parser tests; the ladder test proves fall-through on `NotConfigured`.
- [Big mechanical diff in host] → each file migrated in its own commit; the forge-leak ratchet is the completion check (LEAK section must be empty).
- [Core coverage gate] → parsers move to `forge/model.rs` (covered); transport stays in `github.rs` (ignored). `forge/mod.rs` leaves `cov_ignore` once the dead prototype is gone (trait defaults are tested via the fake).

## Migration Plan

Lands as a sequence of commits on one branch: model move → error/trait → svc impls → host files → deletions → ratchet shrink → docs. Rollback = revert.
