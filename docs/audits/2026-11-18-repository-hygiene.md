# Thegn repository hygiene and development velocity

[Read the report in Linear](https://linear.app/blakeashley/document/repository-hygiene-and-development-velocity-november-18-2026-4ac9ce71e4fd)

**Decision, November 18, 2026.** Start with the build-lane inventory and the PR template correction. The inventory determines how much of the largest storage cost can actually be recovered. The template correction fixes a contributor-facing error in one review. In parallel, establish a terminal regression test for project removal before extracting that behavior from the event loop. The Linear parent tracks seven independently verifiable changes. [THE-2265](https://linear.app/blakeashley/issue/THE-2265)

## Audit baseline

The source and history measurements use `6f37810b2`. Disk sizes and worktree counts are observations from one developer host on November 18. They change during builds. The CI timing comes from an earlier workflow run recorded in the workflow itself. It establishes why another measurement is worthwhile. [Audited source revision](https://github.com/blakeashleyjr/thegn/commit/6f37810b23f94323cf384e5d9bfd49c857aa2c47) · [CI workflow](../../.github/workflows/ci.yml)

| Area            | Observation                                                                                                                                                           | Use in prioritization                                                                                                              |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| Local storage   | `target/` occupied 230 GiB, including 129 GiB in `target/lanes/` and 86 GiB in `target/debug/`. Git listed 54 worktrees, one locked and zero marked prunable.         | Lanes accounted for roughly 56% of the sampled target size. Eligibility needs to be established per directory.                     |
| PR feedback     | PRs run fmt, lint, test, and smoke. Lint also checks formatting. The workflow records 5.4 minutes for a historical fmt job that did about five seconds of formatting. | Capturing current setup and queue times is cheap enough to do before changing a required check.                                    |
| Shared code     | `crates/thegn-host/src/run.rs` had 29,596 lines and appeared in 146 non-merge commits in the sampled month. Two successive project-removal fixes edited it.           | Workspace removal offers a concrete first extraction with observable behavior. Commit frequency is a proxy for contention.         |
| Search scope    | Of 5,328 tracked paths, 749 were in `docs/audits/`, 838 in `openspec/changes/archive/`, and 845 in `.thegn/pipeline/`.                                                | About 46% of tracked paths belong to these evidence collections. Scope searches according to the question and measure the results. |
| UI verification | The host has a unit test for final-project removal. The visual suite is opt-in, and the development guide records stale baselines and timeouts.                       | A small real-terminal scenario can cover the selection, action, and resulting screen together.                                     |

## Work packages

### 1. Identify recoverable build output, then reclaim it

The existing disk command reports worktree totals. An operator seeing a large `target/lanes/` directory needs more information before choosing what to reclaim. Inventory each lane's bytes, verifiable owner, activity, lock state, and reason to retain or reclaim it. Measure the cost of the scan itself. Unknown ownership is an actionable finding. The broader main-checkout generation problem already has an owner. [Disk command](../../crates/thegn-host/src/cmd/disk.rs) · [Existing disk issue THE-1997](https://linear.app/blakeashley/issue/THE-1997)

Only after the inventory shows eligible lanes should the reclaim work proceed. Recheck identity, ownership and activity when applying a dry run because a new build can start between the two operations. Exercise a newly active lane, a held lock, an interrupted run, and a retry. Report actual GiB recovered alongside the next comparable build time. If little is eligible, record why before changing retention policy. [Inventory THE-2279](https://linear.app/blakeashley/issue/THE-2279) · [Reclamation THE-2284](https://linear.app/blakeashley/issue/THE-2284)

### 2. Correct CI instructions and price the formatting job

The PR template tells contributors to put `[ci-macos]`, `[ci-windows]`, or `[ci-e2e]` in commit messages. The workflow uses a manual dispatch with `extras`. Correct the template now and verify that the documented way to launch and find the optional run works for a chosen ref. [PR template](../../.github/pull_request_template.md) · [Instructions THE-2280](https://linear.app/blakeashley/issue/THE-2280)

For consolidation, compare current runner minutes and time to first actionable failure with the proposed arrangement. Separate queue wait, provisioning, and execution. Inspect branch protection and required status names first. The runner-policy test explicitly pins the four PR jobs. Coordinate that change with the workflow and template, retain one effective format check, and demonstrate that an unformatted change fails. A lower bill with materially slower failure feedback warrants revisiting the design. [Runner-policy test](../../packaging/tests/test_ci_runner_policy.py) · [Previous CI decision THE-1846](https://linear.app/blakeashley/issue/THE-1846) · [Consolidation THE-2281](https://linear.app/blakeashley/issue/THE-2281)

### 3. Test project removal through a terminal, then extract the decision

Recent changes corrected which project the palette targets, what happens when the active project is removed, and whether removing the last project quits the application. Build a test with two disposable projects and isolated application state. Remove the selected inactive project, verify the survivor is usable, remove the survivor, and prove that the splash accepts input. Repeat the focused scenario on a Linux CI-like worker. Record runtime and stability before adding it to routine smoke coverage. [Existing host tests](../../crates/thegn-host/src/run_tests.rs) · [PTY smoke harness](../../test/pty-smoke.sh) · [Regression test THE-2283](https://linear.app/blakeashley/issue/THE-2283)

Use that test to protect a small extraction. Move project-removal decisions and their focused tests together, reusing the workspace switch helpers where they already own a decision. Keep the event loop responsible for orchestration. Review the real callers, event ordering, and wake/render behavior on the combined tree. The broader compositor redesign remains tracked separately. [Workspace switch helpers](../../crates/thegn-host/src/workspace_switch_prep.rs) · [Extraction THE-2285](https://linear.app/blakeashley/issue/THE-2285) · [Compositor issue THE-205](https://linear.app/blakeashley/issue/THE-205)

### 4. Give agents two short search paths

For an implementation question, search the relevant crate, tests and current spec first. Include the active change when investigating unfinished work. For a question about a past decision, start from the issue ID and use its handoff or archived design. Compare matching-file counts for the same literal and verify the first path finds the actual implementation. Verify separately that the historical route still finds the cited evidence. The path counts establish the opportunity. Token or elapsed-time savings require a task-level measurement. [Agent workflow](../agent-workflow.md) · [Search scope THE-2282](https://linear.app/blakeashley/issue/THE-2282)

## Delivery order and closeout

| Start          | Work                             | Evidence to attach in Linear                                                                                          |
| -------------- | -------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| First          | THE-2279 and THE-2280            | Sample lane inventory with eligible bytes and scan time. Correct instructions verified against the workflow.          |
| Independent    | THE-2281, THE-2282, and THE-2283 | Comparable CI runs and required-status plan. Search results for code and history. Repeated PTY runs and elapsed time. |
| After THE-2279 | THE-2284                         | Dry-run and apply results, skip reasons, recovered space, and warm rebuild cost.                                      |
| After THE-2283 | THE-2285                         | Focused host and PTY results, the new owner of removal decisions, and remaining event-loop callers.                   |

At closeout, record a measured result or a decision to stop for each experiment. Use the first ten comparable accepted changes to check for unintended effects on time to the first passing test and combined-tree repair rounds. The existing token baseline covers issue-attributed Codex sessions only. It provides a comparison for that same population. [Measurement procedure](../agent-workflow.md#measure-accepted-work)

## Reproducing the baseline

Run the source counts at the audit revision. Run disk and worktree measurements on the host under investigation and record the timestamp. Keep each storage sample together because active builds can shift gigabytes between readings.

```sh
git ls-tree -r --name-only 6f37810b2 | wc -l
git ls-tree -r --name-only 6f37810b2 -- docs/audits openspec/changes/archive .thegn/pipeline | wc -l
git show 6f37810b2:crates/thegn-host/src/run.rs | wc -l
git log --since='2026-10-19 18:00:00 -0700' --no-merges --format= --name-only 6f37810b2 -- crates | rg -xc 'crates/thegn-host/src/run\.rs'
du -h -d1 target
git worktree list --porcelain
```
