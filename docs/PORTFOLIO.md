# Thegn Delivery Portfolio

This is the live repository-side index for the Thegn team's delivery portfolio.
It was reconciled against `main`, OpenSpec, tests, public documentation, and
Linear on 2026-09-09 (America/Los_Angeles). Linear owns workflow state and full
acceptance criteria; this file owns the durable cross-links and architectural
context that must remain reviewable with the code.

The pre-reconciliation evidence and issue-by-issue audit is preserved in
[`docs/audits/linear-reconciliation-2026-09-08.md`](audits/linear-reconciliation-2026-09-08.md).
The ongoing lifecycle and closure rules are in
[`docs/delivery-governance.md`](delivery-governance.md).

## Portfolio at a glance

- [Thegn Alpha Readiness](https://linear.app/blakeashley/initiative/thegn-alpha-readiness-457f46d97d92)
  is the release-bound initiative.
- [Thegn Post-Alpha Roadmap](https://linear.app/blakeashley/initiative/thegn-post-alpha-roadmap-61e4a807e68f)
  owns deferred greenfield, residual, verification, and retirement work.
- `python3 scripts/delivery_state.py report` derives current change, lifecycle,
  disposition, project-link, and task totals from the checked-in delivery index.
- Add `--linear-json <read-only-export.json>` to compare a separately exported
  Linear snapshot without making the default report depend on the network.
- Every open issue has a project, milestone, priority, and bounded outcome.
- Every In Progress and Todo issue is assigned to Blake Ashley; unassigned
  backlog work receives an issue owner when it is promoted for execution.
- There are no cycles or invented delivery dates. Project order expresses
  dependency and readiness; it does not promise a calendar schedule.

| Alpha project                                                                                                                            | State       | Priority | Outcome                                                                                               |
| ---------------------------------------------------------------------------------------------------------------------------------------- | ----------- | -------- | ----------------------------------------------------------------------------------------------------- |
| [Alpha Reliability & State Compatibility](https://linear.app/blakeashley/project/alpha-reliability-and-state-compatibility-d52ec69b3c57) | In Progress | Urgent   | Shared state fails closed, compatibility is operation-aware, and important runtime modes are visible. |
| [Client API & Remote Access](https://linear.app/blakeashley/project/client-api-and-remote-access-fb8407005605)                           | Planned     | High     | Remote control has an explicit trust boundary and published client contracts are truthful.            |
| [Plugin UI Platform](https://linear.app/blakeashley/project/plugin-ui-platform-cc32c0028759)                                             | Planned     | Medium   | Plugin vocabulary, permissions, documentation, and actual UI extension points agree.                  |
| [Distribution & Release Readiness](https://linear.app/blakeashley/project/distribution-and-release-readiness-5c0fd9d8b55e)               | Planned     | High     | Supported platform bundles and publication channels are reproducible and rehearsed.                   |
| [Spec & Tracker Reconciliation](https://linear.app/blakeashley/project/spec-and-tracker-reconciliation-a8a7287d5d61)                     | In Progress | High     | Accepted specs, active proposals, public docs, and Linear cannot silently disagree about delivery.    |

Post-alpha greenfield capabilities default to Medium. High is reserved for a
demonstrated shipped-path correctness or security failure, billable-resource
leak risk, or release/support evidence. Low is reserved for blocked speculative
work and proposal retirement.

| Post-alpha project                                                                                                                   | State   | Priority | Outcome                                                                                                                     |
| ------------------------------------------------------------------------------------------------------------------------------------ | ------- | -------- | --------------------------------------------------------------------------------------------------------------------------- |
| [Agent & Workflow Platform](https://linear.app/blakeashley/project/agent-and-workflow-platform-a0f5c2e165d2)                         | Planned | Medium   | Harness, tracker, pipeline, CI, review, and SCM capabilities share explicit foundations and dependency order.               |
| [Workspace Product & Observability](https://linear.app/blakeashley/project/workspace-product-and-observability-fb0f99380fed)         | Planned | Medium   | Multi-repo navigation, search, dashboards, terminal UX, media, voice, and visible state evolve as independent product work. |
| [Runtime Security & Host Architecture](https://linear.app/blakeashley/project/runtime-security-and-host-architecture-8dfde99f448e)   | Planned | High     | Credential, containment, crash, host, session, proxy, and provisioning residuals are separated by risk.                     |
| [Cross-Platform Support & Verification](https://linear.app/blakeashley/project/cross-platform-support-and-verification-ec2b30969755) | Planned | Medium   | Windows, sandbox, devcontainer, provider, mount, and completion claims require real-platform evidence.                      |
| [Proposal Retirement & Cleanup](https://linear.app/blakeashley/project/proposal-retirement-and-cleanup-9bef1d0ad6ac)                 | Planned | Low      | Delivered, superseded, duplicate, and obsolete OpenSpec records are retired without becoming false feature commitments.     |

## Current architectural truth

### API and remote clients

The control-plane architecture has a central capability catalog, stable scope
and error vocabularies, generated schema, transport projections, and a
shrink-only surface-gap ratchet. Unix connections now verify peer credentials
before granting implicit local administration. Remotely reachable endpoints
must use an explicitly declared TLS-terminated or tunnel topology; direct
non-loopback plaintext requires a conspicuous unsafe opt-in. It is still not
feature-equivalent across HTTP, gRPC, CLI, MCP, and plugins. Browser driving is
deliberately absent until a provider-backed session lifecycle exists; the
generic observer feed does not carry terminal snapshot/delta bytes; and remote
enqueue remains incomplete for true remote worktrees.

The honest product claim is therefore **useful bounded client API**, not
**complete native-client parity**. The governing contract is
[`docs/superpowers/specs/control-api.md`](superpowers/specs/control-api.md), with
accepted behavior in [`openspec/specs/control-plane/spec.md`](../openspec/specs/control-plane/spec.md).

### Plugin and UI extensibility

The UI is configurable, but it is not fully pluggable or arbitrarily editable
through plugins. Runtime plugins can currently contribute statusbar segments,
notification sources, palette actions, and issue providers. The general host
does not yet accept panel sections, sidebar tabs, themes, layout/chrome,
key zones, drawers, overlays, or top-level apps. `PanelSection` is reserved
v0.3 wire vocabulary rather than a wired runtime surface. The calendar
`DataSource` adapter is a specialized provider seam, not a general UI host.

The v0.3 contract now derives its advertised extensions, host calls, scope
lattice, loader grants, and generated schema from canonical matrices. THE-102,
THE-107, and THE-108 own explicit product/runtime growth rather than allowing
config-driven features such as themes or drawers to be mistaken for arbitrary
plugin UI support. See
[`docs/help/plugins.md`](help/plugins.md).

### Configuration

Configuration remains one of the strongest subsystems: typed models, strict
validation, layered provenance, trust clamps, generated references, and hot
reload. Shared-state migration now fails closed without explicit authority,
operations declare the schema contract they require, and the TUI preserves a
typed sticky refusal without discarding its last good model. Sandboxed compiler
caching is explicitly `off` or `auto`, trusted-config only, fail-soft, and
diagnosable. THE-51 retains the bounded localization/config integration work;
broad rewrites of the configuration system are not currently justified.

## Open issue ledger

The descriptions and acceptance criteria in Linear are normative for workflow.
The “spec/change” column identifies the repository artifact that owns or is
being created for the behavior. An active change is a proposal, not proof that
the work shipped.

### Alpha Reliability & State Compatibility

| Milestone                                 | Issue                                                 | State / priority | Remaining outcome                                                                                               | Spec/change                                                                               |
| ----------------------------------------- | ----------------------------------------------------- | ---------------- | --------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| R2: Contained toolchain and visible state | [THE-97](https://linear.app/blakeashley/issue/THE-97) | Backlog / Medium | Continuously prove that a staged agent can commit without escaping the configured worktree sandbox.             | [`verify-stage-worker-containment`](../openspec/changes/verify-stage-worker-containment/) |
| R2: Contained toolchain and visible state | [THE-93](https://linear.app/blakeashley/issue/THE-93) | Backlog / Medium | Render the persisted flat/grouped sidebar mode continuously, including narrow layouts.                          | [`make-sidebar-mode-visible`](../openspec/changes/make-sidebar-mode-visible/)             |
| R2: Contained toolchain and visible state | [THE-92](https://linear.app/blakeashley/issue/THE-92) | Backlog / Low    | Classify active connectivity and render truthful Ethernet, Wi-Fi, or generic fallback semantics cross-platform. | [`detect-active-network-kind`](../openspec/changes/detect-active-network-kind/)           |

The shared-state safety and sandbox cache remediations are complete. THE-97 is
the separate continuous worker-containment proof, not a substitute for either.

### Client API & Remote Access

| Milestone                        | Issue                                                   | State / priority | Remaining outcome                                                                                                                   | Spec/change                                                                                         |
| -------------------------------- | ------------------------------------------------------- | ---------------- | ----------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| C1: Remote control security      | [THE-41](https://linear.app/blakeashley/issue/THE-41)   | Backlog / High   | Close the remote-access epic only after its four bounded children establish the transport, identity, routing, and parity decisions. | Children THE-98–THE-101                                                                             |
| C1: Remote control security      | [THE-98](https://linear.app/blakeashley/issue/THE-98)   | Backlog / Medium | Persist a generated surface map and make explicit product-parity decisions for remote consumers.                                    | [`audit-remote-surface-map`](../openspec/changes/audit-remote-surface-map/)                         |
| C1: Remote control security      | [THE-99](https://linear.app/blakeashley/issue/THE-99)   | Backlog / High   | Complete `route_to_host` enqueue semantics for true remote worktrees.                                                               | [`add-remote-enqueue-modes`](../openspec/changes/add-remote-enqueue-modes/)                         |
| C2: Client contract truthfulness | [THE-104](https://linear.app/blakeashley/issue/THE-104) | Backlog / Medium | Decide and publish whether external IDEs have an inbound handoff/control contract.                                                  | [`define-external-ide-inbound-boundary`](../openspec/changes/define-external-ide-inbound-boundary/) |
| C2: Client contract truthfulness | [THE-105](https://linear.app/blakeashley/issue/THE-105) | Backlog / High   | Publish a truthful observer bootstrap/filter contract, including which stream owns terminal snapshots and deltas.                   | [`publish-observer-event-contract`](../openspec/changes/publish-observer-event-contract/)           |

THE-41 is an epic, not a second implementation of its children. C1 must precede
claims of safe remote operation; C2 may proceed in parallel because it narrows
published promises to what current surfaces can actually do.

### Plugin UI Platform

| Milestone               | Issue                                                   | State / priority | Remaining outcome                                                                                                               | Spec/change                                                                                       |
| ----------------------- | ------------------------------------------------------- | ---------------- | ------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| P1: Contract baseline   | [THE-51](https://linear.app/blakeashley/issue/THE-51)   | Todo / Medium    | Complete runtime TUI localization and define how plugin-provided user-visible strings participate.                              | [`extend-localization-surfaces`](../openspec/changes/extend-localization-surfaces/)               |
| P2: Runtime UI surfaces | [THE-108](https://linear.app/blakeashley/issue/THE-108) | Backlog / Medium | Wire the already-declared `PanelSection` contribution end to end with lifecycle, safety, and tests.                             | [`add-ui-component-contract`](../openspec/changes/add-ui-component-contract/)                     |
| P2: Runtime UI surfaces | [THE-107](https://linear.app/blakeashley/issue/THE-107) | Backlog / Medium | Decide bounded contracts for sidebar, themes, and key zones without promising arbitrary rendering.                              | [`decide-plugin-ui-extension-surfaces`](../openspec/changes/decide-plugin-ui-extension-surfaces/) |
| P2: Runtime UI surfaces | [THE-102](https://linear.app/blakeashley/issue/THE-102) | Backlog / Medium | Decide and expose a provider-neutral debugger adapter/DAP surface; do not treat the BugStalker audit as broad debugger support. | [`define-debugger-adapter-surface`](../openspec/changes/define-debugger-adapter-surface/)         |

The v0.3 baseline is truthful before P2 grows it. Runtime extension points must
remain bounded typed contributions; this project is not a promise of an
unrestricted DOM, terminal-frame, or arbitrary-code UI layer.

### Distribution & Release Readiness

| Milestone                 | Issue                                                 | State / priority | Remaining outcome                                                                                                              | Spec/change                                                                             |
| ------------------------- | ----------------------------------------------------- | ---------------- | ------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------- |
| D1: Platform bundles      | [THE-15](https://linear.app/blakeashley/issue/THE-15) | Todo / High      | Define and build complete, supported platform bundles with terminal, fonts, dependencies, config, and clean-host verification. | [`add-batteries-included-bundles`](../openspec/changes/add-batteries-included-bundles/) |
| D2: Publication rehearsal | [THE-52](https://linear.app/blakeashley/issue/THE-52) | Todo / High      | Produce reproducible artifacts, metadata, attestations, and rehearsed publication evidence for each supported channel.         | [`add-package-manager-releases`](../openspec/changes/add-package-manager-releases/)     |

THE-15 defines what a supported platform receives; THE-52 defines how that
artifact is published. They are related but neither is evidence that every
possible operating system or package manager is supportable.

### Spec & Tracker Reconciliation

| Milestone             | Issue                                                 | State / priority     | Remaining outcome                                                                                                                              | Spec/change                                                         |
| --------------------- | ----------------------------------------------------- | -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| S1: One-time truth-up | [THE-21](https://linear.app/blakeashley/issue/THE-21) | In Progress / Medium | Make the shipped typed-event/catalog-action automation model normative and explicitly retain or reject missing triggers and operator controls. | [`add-automation-rules`](../openspec/changes/add-automation-rules/) |

## Whole-repository OpenSpec state

The alpha initiative does not silently absorb the entire product idea backlog.
Every directory currently under `openspec/changes/` is intentionally classified
in [`delivery/index.json`](../delivery/index.json). The index distinguishes
proposed, active, delivered-awaiting-archive, superseded, and
verification-gated work. Live totals are derived with
`python3 scripts/delivery_state.py report`; this document does not copy them.

The alpha changes map to the issue ledger above. The shipped-path signing fix
is complete; `add-scm-workflow-customization` remains active only for four
separately bounded, deferred greenfield workflows. The prior outside-Linear
change set is represented in the Post-Alpha Roadmap rather than silently
expanding alpha scope.
Overlapping or superseded changes share a consolidation/retirement owner rather
than producing duplicate work.

Medium is the normal priority for comparable greenfield capabilities. High is
limited to demonstrated pipeline correctness, credential custody,
billable-provider leak safety, and Windows release/support evidence. No
post-alpha issue is In Progress or Todo, assigned, scheduled, or an alpha
commitment merely because its OpenSpec change is active.

The active-change paths and bounded acceptance criteria are recorded in each
Linear issue description. Their presence in `openspec/changes/` still means
“proposed or partially delivered,” never “currently staffed.” Promotion from
Backlog requires an explicit execution decision; archive requires accepted
behavior and implementation to agree.

These counts are a dated verification record, not a hand-maintained green
badge. Re-run `openspec list --json` and `just openspec-validate` for the live
state. The complete remaining-change classification is
[`docs/audits/openspec-active-portfolio-2026-09-08.md`](audits/openspec-active-portfolio-2026-09-08.md).

## Reconciliation record

The 2026-09-08 truth-up closed 18 issues whose accepted outcome was already on
`main` or was explicitly a completed decision/audit: THE-7, THE-11, THE-13,
THE-17, THE-19, THE-20, THE-22, THE-26, THE-27, THE-32, THE-34, THE-40,
THE-49, THE-55, THE-60, THE-62, THE-69, and THE-91. Each closure includes code,
test, or accepted-spec evidence in Linear. Broader residual promises were split
into THE-97 through THE-109 rather than hidden inside completed work.

The 2026-09-09 remediation completed THE-90, THE-94, THE-95, THE-96, THE-100,
THE-101, THE-103, THE-106, THE-109, and THE-131. Seven standalone OpenSpec
changes were reconciled into accepted specifications; THE-131's completed
signing slice was removed from the still-active shared SCM change without
closing its four Medium-priority greenfield owners.

The reconciliation archived 62 delivered OpenSpec changes. The initial
issue-linked set was:

- `add-agent-task-engine`
- `add-browser-preview-loop`
- `add-calendar-and-world-clock`
- `add-chat-webhook-sinks`
- `add-drawer-tool-registry`
- `add-embedded-skills`
- `add-event-feed-subscriptions`
- `add-external-ide-handoff`
- `add-mcp-proxy-hub`
- `add-mise-toolchain-provider`
- `add-multiplexer-parity`
- `add-pipeline-board-access`
- `add-pr-comments-in-diff`
- `add-pr-queue`
- `add-session-profile-migration`
- `add-submodule-integration`
- `add-theme-builder-overlay`
- `add-tracker-provider-suite`
- `add-watched-pr-comment-tasks`
- `add-worktree-lifecycle-hooks`
- `align-config-formats-and-validation`
- `audit-debugger-capability`
- `complete-control-surface-coverage`
- `make-daemon-default`

The whole-active-universe follow-up then reconciled and archived 38 additional
delivered changes:

- `add-agent-orchestration-surface`
- `add-cli-namespaces-and-remote-open`
- `add-config-trust-resolution`
- `add-container-management`
- `add-cross-host-merge-queue`
- `add-decoupled-identities`
- `add-do-fly-providers`
- `add-env-setup-ux`
- `add-file-manager-seam`
- `add-generic-lsp-registry`
- `add-machine0-provider`
- `add-mcp-write-tools`
- `add-notification-sound-packs`
- `add-ntfy-push-bridge`
- `add-pipeline-board`
- `add-pipeline-config-and-skill`
- `add-pipeline-roster-stages`
- `add-profile-reordering`
- `add-release-channels`
- `add-remote-image-paste`
- `add-semantic-repo-map`
- `add-session-fork`
- `add-sidebar-actions-and-mouse`
- `add-sidebar-folder-ordering`
- `add-sidebar-visual-hierarchy`
- `add-tailnet-host-discovery`
- `add-theme-contrast-contract`
- `add-weather-widget`
- `extend-system-monitoring`
- `fix-attention-signal-noise`
- `fix-drag-grab-precision`
- `fix-land-merged-folder`
- `fix-sidebar-drop-position-semantics`
- `move-merge-queue-ambient-surface`
- `nest-sidebar-pipelines`
- `reclaim-idle-worktree-targets`
- `rename-workspaces-to-projects`
- `stabilize-sidebar-internals`

Additional delivered changes may be archived only after their tasks, proposal,
design, deltas, code, tests, and documentation agree. A checked task list alone
is not delivery evidence.

## Source-of-truth and closure rules

Use the sources in this order; each answers a different question:

1. Code and tests describe what the current revision actually does.
2. Base OpenSpec in `openspec/specs/` defines accepted required behavior.
3. Active OpenSpec in `openspec/changes/` proposes a change and is not delivery
   evidence until reconciled and archived.
4. Linear owns priority, workflow state, dependency, assignee, project, and
   milestone.
5. `tasks.md` is the long-horizon capability map and historical progress log;
   it does not override code or an accepted spec.

An issue may move to Done only when its bounded acceptance criteria are present
on `main`, relevant tests pass, public claims are truthful, and the associated
OpenSpec is either archived or explicitly records the remaining work. A broad
ticket with a shipped slice and a real residual should be split, not partially
closed by prose. A new capability must link its Linear issue in the proposal and
its active OpenSpec path in Linear.

Run `just delivery-check` for the repository/Linear ownership graph and
generated-contract drift gates, and `just openspec-validate` for strict spec
validation.
