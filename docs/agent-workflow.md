# Agent development workflow

Use the root [AGENTS.md](../AGENTS.md) for project instructions. In Claude Code, check the loaded memory files with `/context all`. [Claude Code project instructions](https://code.claude.com/docs/en/memory)

## Find the evidence

Start with one issue and one question. For example, `just agent-context THE-21` finds the automation change and its handoffs. `just agent-context THE-41` explains which children own an epic's specs. An issue outside the reviewed inventory may still have a committed review in `.thegn/pipeline/<issue>/`. The [delivery policy](delivery-governance.md) governs ownership and archive transitions.

Use `codegraph_explore` for cross-crate navigation. Pass the absolute worktree path and a symbol, with two or three files initially. A query for `TerminalWaker poll_input` finds host call sites and the waker spike. Use `rg -n 'poll_input' crates/thegn-host` for the literal sites. Graph edges are candidates. Check trait dispatch, macro expansions and feature-gated code in source when correctness depends on completeness.

For a current implementation question, search the symbol in the local CodeGraph index, then check its literal sites in the relevant source tree. Add the active OpenSpec paths when the behavior is in flight; `just agent-context` reports the issue's current contract and handoff directory:

```sh
just graph-find poll_input
just agent-context THE-21
```

For a past issue decision, use that issue's handoff directory and the specific OpenSpec change it cites. Search archived changes by adding their exact `openspec/changes/archive/<date>-<name>` path; keep the search scoped to the issue and cited contract:

```sh
rg -n '## Decision|RepoConfigFile|AutomationEvent' \
  .thegn/pipeline/THE-21 openspec/changes/add-automation-rules
```

The handoff and OpenSpec files retain the decision and its evidence. `just agent-context` is the issue entry point, not a substitute for reading those sources.

Tried at alpha.125: `rg -l 'poll_input' crates/thegn-host` matched 17 files and included the expected host input sites. The THE-21 search above, run with `rg -l` in place of `rg -n`, matched 7 files across the handoffs and `add-automation-rules`, and found the decision and the automation contract.

### Keep the code index current

Use the CodeGraph CLI on the developer host (tested with 1.6.2). Run `just graph-sync` on the first use of a worktree. Select manual sync if the CLI offers Git hooks on a filesystem without watching. The checkout owns its ignored `.codegraph/` directory. The project MCP server watches while connected on supported filesystems. `just graph-find <symbol>` initializes or incrementally syncs before each CLI lookup, including changes since a branch switch. Use it when MCP or its watcher is unavailable.

After an update or a suspicious result, run `just graph-sync` then `just graph-check`. The check verifies the worktree path, index state, pending file changes, worktree mismatch and extractor version. For the underlying report use `codegraph status --json .`. When a new extractor requires a rebuild, run `codegraph index .` in that worktree and check status again. The older Understand Anything snapshot is a separate historical analysis. Use CodeGraph for current code navigation.

### Check the clients and memory

The project `.mcp.json` declares CodeGraph, AgentMemory, and Linear for interactive Claude Code. Install the matching CLIs on each developer host, then open Claude in the reviewed checkout and approve those named servers in its MCP prompt. Use `claude mcp get codegraph`, `claude mcp get agentmemory`, and `claude mcp get linear` to inspect effective status. Use `/memory` to inspect the loaded project instructions and `/mcp` for Linear authentication. In Codex, inspect `codex mcp list` and the session's exposed tools. Restricted launches may omit project servers or project settings. Use the same issue-context and graph CLI recipes in those sessions.

The AgentMemory CLI and the Codex stdio shim were evaluated at version 0.9.29. Use the same shared service URL for both clients. Run `just memory-check` after changing the service or client configuration. It saves a unique marker to an isolated temporary store, recalls it through a second MCP process, then runs a read-only search against the shared service. The shared check exits unsuccessfully when only local memory works. `just memory-check --offline` exercises standalone persistence by itself. To diagnose the shared service, run `agentmemory status` on its host and check `AGENTMEMORY_URL` and service authentication there.

Save reusable decisions with a searchable `thegn` and issue or capability in the text, a source path and a commit. Include `project=thegn` on saves. Search a few results at a time. The standalone store in the tested version falls back to local keyword recall and writes a single JSON file. Use the committed handoff for concurrent lanes and worktree-to-worktree continuity. Recheck the decision against the cited revision before reusing it. Link corrections to the superseding design, test, or spec.

## Measure accepted work

The baseline CSV (kept out of the public repository) contains ten issues with passing reviews on the main branch. It counts the available Codex sessions whose working directory belonged to the issue-named worktree. The ten rows cover 134 distinct sessions. Each contributes its final cumulative input count minus cached input. The sample covers those Codex worktrees only. Historical transcripts and review records do not consistently establish the first passing focused test time or combined-tree repair rounds. Those fields are blank.

At the start of the next comparable pipeline issues, record the start timestamp and session IDs in the existing dispatch notes. At the first passing focused test, record the timestamp and command. At acceptance, record the evidence commit or review path and count combined-tree fix-and-retest rounds. Include every issue-attributed session using the same harness and worktree scope. For a dedicated set of Codex rollouts, calculate uncached input with:

```sh
python3 scripts/agent_metrics.py --codex-rollouts <session.jsonl> <other-session.jsonl>
```

Append one `after` row per accepted issue to a copy of the baseline, using the same `kind`. Leave missing measurements blank. Use a distinct kind for a different harness or work mix. For example, compare like-sized pipeline changes and keep single-edit fixes in their own cohort. Measure elapsed minutes from work starting to the first passing focused test. Zero repair rounds means the first combined check passed. A blank means the evidence is missing or the measure was inapplicable.

Run `just agent-metrics <csv-path>` after ten comparable issues. Compare medians and the number of measured rows. Consult the linked verdicts for reopened defects or scope changes before crediting a token reduction to the workflow. Host-wide usage cannot identify time or tokens for a single issue.

## Maintenance

The author updates the linked guide in the same change that alters a command, tool, or delivery convention. The offline delivery gate checks local guide links and issue routing. At wave close, the integrator reconciles Linear and the ledger, checks graph freshness on the integration checkout, and reviews any recurring rediscovery or merge repair in the dispatch reports. After a client or memory-service upgrade, repeat the live checks above on a developer host. When parallel lanes change a contract, use the [early integration check](../CONTRIBUTING.md#early-integration-of-dependent-branches) on the combined checkout.
