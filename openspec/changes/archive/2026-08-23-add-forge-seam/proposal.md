## Why

GitHub is reached through four fragmented abstractions (`thegn_core::forge::Forge` — issue-only, one caller; `thegn_svc::gh::GhBackend` — async, not object-safe, one live method; `thegn_svc::prq::PrQueueForge` — the queue's six ops; `thegn_svc::issue::GitHubIssuesBackend`) plus a free-function `gh` layer that the host calls directly at 40 sites in 9 files, with three duplicate `gh auth` probes, four duplicate owner/repo parsers, two connectivity feeds, and a `PanelState↔Result` round-trip. A non-GitHub forge is therefore a rewrite, not an implementation. The audit ranked this the worst seam; phases 0–1 laid the seam vocabulary and the `forge-leak` ratchet that this change burns down.

## What Changes

- **`thegn_core::forge` becomes the forge seam**: `forge/model.rs` (every PR/check/review/issue/diff model type and pure parser, moved out of `github.rs`), `ForgeError` (replaces `GhError`; `impl SeamError`; adds `Unsupported`), `ForgeCaps`, and one **sync, object-safe `Forge` trait** (blocking seam: every op is process-bound today; host already runs them on blocking threads) with optional ops defaulting to `Unsupported`. `PrPanel::from_result` is the one pure place a `Result<PrStatus, ForgeError>` becomes a panel state. The dead issue-only prototype (`GitHubForge`/`ForgejoForge`/`detect_forge`/`forge_for_kind`/`extract_issue_from_branch`) is deleted; its four issue ops join the trait.
- **`thegn_core::github` is the GitHub CLI transport only** (`gh_out`/`gh_run`/classify + `GithubCli: Forge`), pinned as IMPL in the ratchet; nothing else in the workspace names it.
- **`thegn_svc::forge`**: `GithubNative: Forge` (octocrab `pr_status`/`pr_list` + circuit breaker, moved from `gh.rs`; other ops `Unsupported`), `impl Forge for Ladder<dyn Forge>` (native → CLI), `ForgeSet` (per-`[[forges]]` host routing, GitHub default) + `forges_for(cfg)`, probes feeding `thegn doctor`. `gh.rs` and `prq.rs` are deleted; `pr_driver` takes `&dyn Forge`, and gets the fake-forge test the trait was written for.
- **Host migration**: a `forge_handle` (process-global `OnceLock<Arc<ForgeSet>>`, the `sched`/`CIRCUIT` precedent) installed at startup; every `thegn_core::github::*` behaviour site in `hydrate.rs`, `actions.rs`, `cmd/pr.rs`, `cmd/pr_queue.rs`, `handlers/pr_queue.rs`, `pr_driver.rs`, `hydrate_tracker.rs` goes through it; `handlers/onboarding.rs` and `cmd/doctor.rs` raw `gh auth` probes become `Forge::whoami`; type imports move to `forge::model`. The `forge-leak` ratchet shrinks to the IMPL files.
- `ForgeKind::{forgejo,gitea}` stay **reserved** (decision: GitHub-only this phase); a non-GitHub forge is now one `impl Forge` + one `ForgeSet` arm.

## Capabilities

### New Capabilities

- `forge`: the forge seam — one trait, caps ⇔ optional ops, native→CLI ladder, per-host routing, cache semantics, `gh` isolation enforced by `just lint`.

### Modified Capabilities

- `provider-seams`: clarifies the sync-vs-async rule (blocking seams use plain `&self`; the forge is the first such seam) — delta on the in-flight `add-seam-foundation-and-capability-catalog` spec.

## Impact

- `crates/thegn-core/src/{forge/,github.rs,remote.rs}`, `crates/thegn-svc/src/{forge/,gh.rs→deleted,prq.rs→deleted,seam/registry.rs,lib.rs}`, `crates/thegn-host/src/{forge_handle.rs,hydrate.rs,hydrate_tracker.rs,actions.rs,pr_driver.rs,pr_view.rs,panel/*,handlers/{pr_queue,onboarding}.rs,cmd/{pr,pr_queue,issue,doctor}.rs,attention_status.rs,connectivity_gate.rs,diff_view.rs}`, `test/forge-leak-ratchet.txt`, `test/async-trait-ratchet.txt` (gh.rs entry retired).
- No SQLite schema change (`pr_cache` JSON shape unchanged — `PrPanel` serde is preserved). No render-path change (all forge calls stay on blocking threads).
- Roadmap: tasks.md **A.6**, the forge rows of the Z/AV groups; the 2026-08-22 audit plan A1.
