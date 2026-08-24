## 1. Core: models, errors, trait

- [x] 1.1 `forge/model.rs`: move every model type + pure parser out of `github.rs` (PrPanel/PanelState/PrStatus/CheckRun/Bucket/ChecksSummary/ReviewThreadRow/IssueRow/PrHeader/PrSearchRow/PrComment/PrReview/ReviewThread/PrConversation/PrDiff*/CreateOpts/MergeMethod/ReviewState + parse\_* + nwo/owner_repo parsers) with their tests; `PrPanel::from_result`
- [x] 1.2 `forge/error.rs`: `ForgeError` (GhError + Unsupported), `impl SeamError`, `Display`, `describe()`; `github.rs` keeps `pub type GhError = ForgeError`
- [x] 1.3 `forge/mod.rs`: `ForgeCaps`, `PrDepth`, `FetchedPr`, `ForgeIssue`/`CreateIssueOpts`, `trait Forge: Probe` with defaults; delete the dead prototype + `extract_issue_from_branch`
- [x] 1.4 `github.rs` → transport + `GithubCli: Forge` (+ `Probe`), keep `gh_out/gh_run/classify`; one `owner_repo` parser
- [x] 1.5 Tests: `from_result` maps every error class; trait defaults return Unsupported; `ForgeError` classes; coverage stays ≥95%

## 2. Svc: native, ladder, set

- [x] 2.1 `svc/forge/native.rs`: `GithubNative: Forge` (pr_status, pr_list via octocrab + circuit breaker; token resolution; other ops Unsupported); move parsers/tests from `gh.rs`
- [x] 2.2 `svc/forge/mod.rs`: `impl Forge for Ladder<dyn Forge>`; `ForgeSet { by_host, default }`, `for_loc`, `forges_for(cfg)`, probes; `registry::forge_probes` uses it
- [x] 2.3 Delete `svc/gh.rs`, `svc/prq.rs`; `pr_driver` takes `&dyn Forge` (+ `fetch_pr` via `pr_status` + `review_threads`); add `FakeForge` and a `drive_queue` test
- [x] 2.4 Ladder tests: NotConfigured falls through, Auth is final

## 3. Host migration

- [x] 3.1 `forge_handle.rs` (`install`/`get`), installed in `main` for both paths
- [x] 3.2 `hydrate.rs` (8 sites), `hydrate_tracker.rs`, `actions.rs` (10), `handlers/pr_queue.rs` (2), `cmd/pr_queue.rs` (2), `pr_driver.rs` (viewer_login → whoami), `cmd/pr.rs` (14 + describe), `cmd/issue.rs`
- [x] 3.3 `handlers/onboarding.rs` + `cmd/doctor.rs` raw gh → `whoami`; doctor's hand-rolled gh block replaced by the providers section
- [x] 3.4 Type imports → `thegn_core::forge::model` (panel/\*, pr_view, diff_view, connectivity_gate, attention_status, hydrate_tests)
- [x] 3.5 `test/forge-leak-ratchet.txt` shrinks to IMPL entries; `test/async-trait-ratchet.txt` drops gh.rs; `test/ignored-result-ratchet.txt` regenerated

## 4. Docs + gate

- [x] 4.1 `docs/superpowers/specs/control-api.md` untouched; `docs/help/review-a-pr.md`/`pr-queue.md` wording if it names `gh`; tasks.md A.6 / forge rows
- [x] 4.2 `just quick` per crate, targeted tests, `just lint` (ratchets), `just test`, `just coverage`; no e2e
      _(done: clippy clean core/svc/host; thegn-core 2439 + thegn-svc 498 + thegn-host 2023 tests; `just lint` (forge-leak ratchet 21 → 4 impl files); coverage ≥95%; doc-check; smoke + PTY smoke; e2e skipped by policy. Also fixed en route: `keyring_available()` was a synchronous D-Bus round-trip on the first-frame path — a wedged secret service hung launch; now capped at 1.5s.)_
