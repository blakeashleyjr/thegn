# Active OpenSpec Portfolio Classification

This is the post-reconciliation inventory of every OpenSpec change that remains
active on 2026-09-08 (America/Los_Angeles). It complements the live alpha issue
ledger in [`docs/PORTFOLIO.md`](../PORTFOLIO.md).

An active change is a proposal or partially delivered plan. It does not, by
itself, mean the work is staffed, scheduled, accepted, or fully implemented.
Task counts are useful navigation aids but are not delivery evidence.

## Summary

| Disposition           | Changes | Meaning                                                                                                                    |
| --------------------- | ------: | -------------------------------------------------------------------------------------------------------------------------- |
| Alpha delivery        |      21 | Mapped to the 24 open Linear issues, five projects, and ten milestones in the alpha initiative.                            |
| Strategic / unstarted |      16 | Explicit long-horizon capability work; not an alpha commitment.                                                            |
| Residual split        |      15 | A substantial baseline shipped, but real narrower work remains or the active umbrella needs decomposition.                 |
| Superseded / retire   |       5 | The design was replaced, duplicated, or implemented with a materially different accepted shape; do not merge it unchanged. |
| Verification-gated    |       9 | Implementation is substantially present, but a real platform/live verification gate remains.                               |
| **Total active**      |  **66** | Every active change contains at least one unchecked task.                                                                  |

The reconciliation separately archived 62 delivered changes after aligning
their proposal, design, deltas, tasks, code/test evidence, and accepted base
specs. The final dated strict-validation snapshot is 141 passed and zero failed
(66 active changes plus 75 accepted capability specs). Strict validation must
still be rerun for live state; this file is not a generated green badge.

## Alpha delivery changes

These 21 changes are linked issue-by-issue in
[`docs/PORTFOLIO.md`](../PORTFOLIO.md#open-issue-ledger):

- `add-automation-rules`
- `add-batteries-included-bundles`
- `add-package-manager-releases`
- `add-remote-enqueue-modes`
- `add-ui-component-contract`
- `align-plugin-v03-contract`
- `audit-remote-surface-map`
- `decide-plugin-ui-extension-surfaces`
- `define-debugger-adapter-surface`
- `define-external-ide-inbound-boundary`
- `detect-active-network-kind`
- `enforce-delivery-state-drift-gates`
- `enforce-unix-control-peer-identity`
- `extend-localization-surfaces`
- `harden-state-db-compatibility`
- `make-sandbox-build-cache-fail-soft`
- `make-sidebar-mode-visible`
- `publish-observer-event-contract`
- `remove-browser-drive-stub`
- `secure-remote-control-transport`
- `verify-stage-worker-containment`

## Deferred, unscheduled changes

No change below is part of the alpha initiative today. References to Done
Linear issues are historical lineage, not live ownership. Promotion requires a
new or reopened bounded issue with project, milestone, priority, owner, and
acceptance criteria.

### Strategic or unstarted

| Change                           | Tasks | Current disposition                                                                    |
| -------------------------------- | ----: | -------------------------------------------------------------------------------------- |
| `add-agent-harness-seam`         |  0/18 | Broad harness abstraction; historical issue owners are Done.                           |
| `add-ci-logs-and-autofix`        |  0/20 | Full CI-log cache and autofix product proposal; unstarted.                             |
| `add-generic-tracker-model`      |  0/15 | Provider-neutral tracker model without a live delivery owner.                          |
| `add-host-as-resource`           | 19/23 | Host substrate exists; readiness UI, volume seeding, and cloud capability work remain. |
| `add-issue-autopilot`            |  0/13 | Unstarted automation product under a historical Done issue.                            |
| `add-issue-driven-worktrees`     |   0/5 | Unstarted tracker-to-worktree flow without a live owner.                               |
| `add-multi-repo-projects`        | 10/19 | Partial multi-repository project model; substantial product work remains.              |
| `add-observability-dashboards`   | 11/17 | Transforms, templating, editor, SQL, rich telemetry, and alerting remain.              |
| `add-osc-attention-signaling`    |   0/9 | Unstarted terminal attention protocol proposal.                                        |
| `add-pr-review-viewed-stacked`   |   0/6 | Unstarted PR viewed/stacked review behavior.                                           |
| `add-remote-provision-hooks`     |   0/6 | Unstarted remote provisioning hook contract.                                           |
| `add-runtime-session-split`      |  0/15 | Major daemon-owned session architecture; no implementation was found.                  |
| `add-sandbox-policy-engine`      |   0/7 | Major sandbox policy system without a live delivery owner.                             |
| `add-scm-workflow-customization` |  0/28 | Broad SCM customization proposal under a historical Done issue.                        |
| `add-skills-registry`            |   0/7 | Network/database registry distinct from the shipped embedded-skills feature.           |
| `improve-agent-pipeline-v2`      |  5/13 | Substantial dispatch/session work remains under historical Done issues.                |

### Delivered baseline with a real residual split

| Change                                 | Tasks | Current disposition                                                                                          |
| -------------------------------------- | ----: | ------------------------------------------------------------------------------------------------------------ |
| `add-agent-model-env-selection`        |   3/4 | Runtime selection shipped; effective-model board presentation remains.                                       |
| `add-crash-reporting-and-traceability` | 25/28 | Crash core shipped; remote/native fallback notifications and final integration evidence remain.              |
| `add-credential-broker`                | 21/26 | Core broker shipped; legacy consumers, audit sink, delegation, rotation, and docs remain security work.      |
| `add-merge-queue-tui`                  | 18/20 | Main TUI shipped; remaining integration/verification must be kept explicit.                                  |
| `add-oci-runtime-tiers`                | 13/17 | Runtime selection shipped; live krun/KVM proof and secondary-host mapping remain.                            |
| `add-terminal-presets`                 | 14/16 | Preset substrate shipped; remaining integration/verification is deferred.                                    |
| `add-viewers-and-quick-open`           |   7/9 | Viewers/ranking shipped; the file-drag model remains unwired and must be removed or separately completed.    |
| `add-workspace-search-replace`         | 21/25 | Search/replace shipped; optional document/archive/binary preview routes remain.                              |
| `add-workspace-zones`                  |  9/11 | Security/config substrate shipped; sidebar, palette, and detail discoverability remain.                      |
| `audit-sandbox-cross-platform`         | 16/20 | Matrix and isolation floor shipped; real smolvm Linux/macOS verification remains.                            |
| `harden-pipeline-slot-accounting`      | 35/41 | Obsolete umbrella; extract smoke, warning, metrics, and infra residuals before archiving its delivered core. |
| `make-usage-overlay-scannable`         |  0/10 | Scannable UI shipped despite stale tasks; proposed public snapshot/API surfaces did not.                     |
| `resurrect-model-proxy`                | 32/34 | Proxy restoration shipped; remaining end-to-end evidence and final gate stay explicit.                       |
| `rework-shell-completions`             | 26/30 | Core completion rewrite shipped; packaging/build verification and coordinated cleanup remain.                |
| `verify-sandbox-mounts`                | 19/21 | Preflight/remedies shipped; Docker Desktop and WSL fixtures remain unverified.                               |

### Superseded or retire without merging unchanged

| Change                      | Tasks | Current disposition                                                                                                           |
| --------------------------- | ----: | ----------------------------------------------------------------------------------------------------------------------------- |
| `add-fleet-view`            |   0/9 | Explicitly superseded by the shipped pipeline board; retire its stale deltas.                                                 |
| `add-localization`          |   0/5 | Substrate was absorbed into `extend-localization-surfaces`; do not keep a second alpha plan.                                  |
| `add-voice-mode`            |  0/30 | Draft cpal/whisper design differs from the accepted command-backed experimental provider. Rewrite accepted v1 before archive. |
| `expand-media-surfaces`     |  0/17 | Overlay/cava draft differs from the accepted docked panel and provider seam. Rewrite delivered v1 first.                      |
| `package-shell-completions` |   0/6 | Duplicates `rework-shell-completions`; consolidate rather than implement both.                                                |

### Verification-gated

| Change                              | Tasks | Current disposition                                                                     |
| ----------------------------------- | ----: | --------------------------------------------------------------------------------------- |
| `add-vps-providers`                 | 16/17 | Requires real Hetzner-account verification before its provider claim is accepted.       |
| `add-windows-ci-distribution`       |  8/10 | Windows build/release CI and coordinated archive remain.                                |
| `add-windows-compositor-validation` |  7/17 | Ten real Windows/on-machine checks remain.                                              |
| `add-windows-daemon-ipc`            | 10/11 | Requires a real Windows two-terminal daemon-race proof.                                 |
| `add-windows-job-objects`           | 10/11 | Requires Windows CI proof for job containment.                                          |
| `add-windows-native-compile`        | 20/23 | Windows CI, full integration, and real waker behavior remain.                           |
| `add-windows-parity`                |  9/10 | Requires on-machine activity/attention validation.                                      |
| `complete-devcontainer-support`     | 18/19 | One scoped host test remains blocked; do not equate config presence with support.       |
| `mark-unverified-backends`          | 12/13 | Truthful warning shipped; enablement requires actual smolvm/WSL verification elsewhere. |

## Promotion and cleanup rules

- Strategic work stays deferred until explicitly prioritized; a stale Done issue
  must not act as its owner.
- Split changes must preserve the shipped base while moving each real residual
  into a bounded plan. Do not archive an umbrella that would erase the residual.
- Superseded changes are rewritten to the accepted implementation or retired;
  stale deltas are never merged merely to empty the active directory.
- Verification-gated work stays active until the named environment was actually
  exercised and the evidence is durable.
- Alpha work is governed by Linear and the linked issue ledger; this deferred
  inventory must not silently expand alpha scope.
