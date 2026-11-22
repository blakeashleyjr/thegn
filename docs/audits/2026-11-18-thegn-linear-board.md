# Thegn Linear board review

**18 November 2026.** The first product release keeps the stock coding agent in the developer’s terminal and moves a real build or test to the project’s selected Fly Sprite or Vercel Sandbox. The user keeps the existing checkout, terminal and model login. The release gate measures laptop CPU offload and checks how each provider handles output, cancellation, result delivery and a failed run. [Release gate](https://linear.app/blakeashley/issue/THE-2419) · [Architecture](../rfcs/agent-execution.md)

The audit covers all 2,437 Thegn issues at the cutoff, including the 15 browser-preview issues created during the review. The final register contains 567 open issues and 1,870 terminal records. Each open issue has a project and priority. Each of the 39 projects belongs to at least one of seven initiatives. The 38 projects with open work each have a handoff document in Linear. Proposal Retirement & Cleanup has no open issues. [Linear audit and project handoffs](https://linear.app/blakeashley/document/thegn-board-audit-and-agent-delivery-index-18-november-2026-8077cd7bcb69) · [Full issue register](2026-11-18-thegn-linear-register.csv) · [Download the register from the audit issue](https://linear.app/blakeashley/issue/THE-2424)

## Start the terminal release

| Ready in parallel                                         | Deliverable                                                                                                                                                |
| --------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [THE-2333](https://linear.app/blakeashley/issue/THE-2333) | Trace the shell entry points stock clients actually use. Capture direct executables, ordinary shells, pipes and cancellation before selecting the adapter. |
| [THE-2316](https://linear.app/blakeashley/issue/THE-2316) | Define the portable command, run and event receipts that identify the job in the terminal and on the worker.                                               |
| [THE-2416](https://linear.app/blakeashley/issue/THE-2416) | Resolve a project’s runner rule. Preserve argv, cwd and local login. A forced remote command reports a refusal if its selected runner cannot start.        |
| [THE-2417](https://linear.app/blakeashley/issue/THE-2417) | Run a pinned build or test on the selected Fly Sprite. Return streams, outcome and resource evidence to the local terminal.                                |
| [THE-2418](https://linear.app/blakeashley/issue/THE-2418) | Deliver the equivalent command lifecycle on the selected Vercel Sandbox, including cleanup and provider evidence.                                          |

All five are High priority, Todo, with zero open blockers. The joint release also depends on sealed workspace inputs and durable local dispatch. Cloud credentials, authorized live spending and provider observations belong to their qualification runs. Hibernation and a hosted control plane have independent delivery gates. [Sealed inputs](https://linear.app/blakeashley/issue/THE-2317) · [Dispatch](https://linear.app/blakeashley/issue/THE-2318) · [Provider release evidence](https://linear.app/blakeashley/issue/THE-2419)

## How older work was evaluated

The initial inventory contained 2,417 issues, of which 551 were open. The review covered every open description and its relations, available recent handoffs, all existing project charters, initiative membership and terminal metadata. The main source baseline was `03789646de36d861342298c5e2a7382ae7081511`. For a historical claim, the register distinguishes a reachable commit, an existing source path, a prior discussion and a current acceptance decision. A source path gives an implementer a place to look. A previous handoff records what was known on its date. The audit used code and verification results when changing a closure decision.

Of 227 originally open issues below THE-2000, 146 had children and 140 had only terminal children. An umbrella can still own cross-slice behavior, a measured result, a hardware run or a decision. The tickets now preserve concrete residuals where earlier handoffs supplied them. For example:

| Issue                                                     | Decision and evidence                                                                                                                                                                                 |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [THE-446](https://linear.app/blakeashley/issue/THE-446)   | Private capture staging landed in `49c3d082a`. Continue from that code for capture coherence, fencing and publication requirements.                                                                   |
| [THE-442](https://linear.app/blakeashley/issue/THE-442)   | The published manifest verifier exists. Verify the outstanding publication and durability cases against that implementation.                                                                          |
| [THE-673](https://linear.app/blakeashley/issue/THE-673)   | The native-versus-CLI benchmark harness exists. The issue remains open for paired measurements and the dependency decision.                                                                           |
| [THE-844](https://linear.app/blakeashley/issue/THE-844)   | Linux evidence covers one part of the original acceptance. Record the outstanding macOS and Windows evidence.                                                                                         |
| [THE-751](https://linear.app/blakeashley/issue/THE-751)   | The default calendar-ingest control method still returns unimplemented. Consolidated the later Lead calendar request, THE-2218, into this issue and retained its acceptance.                          |
| [THE-2092](https://linear.app/blakeashley/issue/THE-2092) | Closed. Main contains the channel clamp and checked OpenSpec task. Checked the stable/dev semantics in source, the README and help text. Two channel tests passed from the existing core test binary. |
| [THE-2130](https://linear.app/blakeashley/issue/THE-2130) | Closed. Verified the current architecture table, owners of the sandbox/provider replacements, the gate reference, network wording and disappearance of both superseded references.                    |

The source history contained 46 open Sandbox & Remote tickets with an exact workstream key in a merged commit. Fifteen checked-off implementations moved into review for their remaining acceptance evidence. Two verified tickets closed. The other 29 now identify landed work, including probe scripts that still need actual platform or provider results. Twelve canceled blocker links were removed across THE-152, THE-153, THE-207 and THE-225. The final 607 open dependency edges form an acyclic graph. No open issue has a canceled or duplicate blocker.

## Project ownership and priorities

The initial board had 169 open issues without a project, 17 without a priority and four projects without an initiative. Three delivery projects were added under the existing Post-Alpha roadmap. Git worktrees and merge integrity owns safe branch and checkout changes. Pipeline dispatch and automation reliability owns dispatch, recovery and worker coordination. Repository maintenance and build feedback owns developer feedback, build footprint and dependency measurements. Each has its own outcome and testable completion criteria. The seven existing initiatives already cover these goals. The browser-preview project created during the review fits the same roadmap and retains its five release milestones.

| Open status | Count | Priority | Count |
| ----------- | ----: | -------- | ----: |
| Todo        |    71 | Urgent   |    84 |
| In progress |    42 | High     |   222 |
| In review   |    42 | Medium   |   193 |
| Backlog     |   412 | Low      |    68 |

Urgent issues include active integrity, credential, containment and reliability risks. Product ordering comes from release gates and their prerequisites. Owner-only actions, including a release push, subscription renewal or paid-provider authorization, remain explicit in their issues.

## Hibernation evidence

The three implementations address different operating conditions. The source review used pinned upstream code and tests. Performance, subscription behavior and host release must be measured by the corresponding Thegn experiment.

- The drkh-n wrapper continues draining the live Claude PTY behind a lock screen. Its screen parsing gives Thegn a fixture for visually idle output while the process and allocation remain resident. [PTY implementation](https://github.com/drkh-n/claude-hibernate/blob/d3a4afac216e5dc286ca2f4504eee574c328a742/claude_hibernate/wrapper.py#L185)
- SteveVitali combines hook or explicit native IDs with a time-based discovery fallback, atomic manifest replacement and a local duplicate-process check. Thegn must prove the exact ID and serialize concurrent cross-host wakes before supplying input. [Discovery and save](https://github.com/SteveVitali/claude-hibernate/blob/65688ec0ed60ce3bd39f6784ea64ab8cef6661bd/scripts/claude-hibernate.sh#L419) · [Wake](https://github.com/SteveVitali/claude-hibernate/blob/65688ec0ed60ce3bd39f6784ea64ab8cef6661bd/scripts/claude-hibernate.sh#L943)
- Tabnap rechecks the process identity, screen and native session immediately before quitting. Its memory figure comes from a pre-exit sample. Thegn should measure post-exit RSS, any remaining shared Codex service and any billed remote allocation. [Engine](https://github.com/rhuthik/tabnap/blob/ea13260cc63ed29d4a01e90579b4b4ddc68f8b3d/src/tabnap/engine.py#L351) · [Codex identity](https://github.com/rhuthik/tabnap/blob/ea13260cc63ed29d4a01e90579b4b4ddc68f8b3d/src/tabnap/agents/codex.py#L68)

M3 now has separate Low-priority experiments for Claude exact-session restart, Codex exact-thread restart and private VM restore. Compare memory snapshots only after at least one native restart path passes. Qualify at 1 minute, 1 hour and 48 hours, including changed networks, expired credentials, competing wakes and a dirty local checkout. [Parent experiment](https://linear.app/blakeashley/issue/THE-2420) · [Claude](https://linear.app/blakeashley/issue/THE-2421) · [Codex](https://linear.app/blakeashley/issue/THE-2422) · [VM comparison](https://linear.app/blakeashley/issue/THE-2423)

## Reading the register

`linear_project` is actual tracker membership. For 1,526 closed records with no historical project, `audit_domain` supplies a search aid without changing any project’s historical completion count. Of those records, 944 inherit a known ancestor project, 241 have individually or thematically reviewed topics, and 341 use a title-based topic rule. The `domain_basis` column identifies which method was used. The recorded status always takes precedence over a topic classification.

The CSV links each issue, any available prior discussion and reachable handoff commit. The child counts describe the inventory at the cutoff. The source-entry column contains existing files on the audited main revision. Active blockers use issue IDs. The 38 Linear project handoffs also link directly to the issues and their qualifying evidence. A historical Done state is preserved as a tracker record; this audit did not rerun every closed issue’s acceptance suite.

The governance schema validator passed. The read-only comparison of the live export with the repository’s 66 reviewed delivery owners returned 175 discrepancies: 19 terminal owners, five project mismatches and 151 missing active entries within the governed project lists. THE-2440 owns reconciliation of the two checked-in inventory files and their fixtures, including the treatment of duplicate status. [Repository governance](../delivery-governance.md) · [Reconciliation issue](https://linear.app/blakeashley/issue/THE-2440)
