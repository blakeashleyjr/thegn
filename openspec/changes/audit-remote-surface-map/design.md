# Design — durable remote-access audit

## Artifact shape

The repository artifact is versioned with code and begins with audit date,
commit, scope, owner, methodology, and next refresh trigger. It contains two
linked tables:

1. A surface inventory organized by architecture layer.
2. A product-decision matrix organized by user capability.

Stable row IDs let Linear issues and later audits refer to a finding without
depending on section prose. Evidence uses repository paths/tests/docs and
linked bounded issues; unknown is an explicit value, never inferred support.

## Surface inventory fields

Every row records: layer/surface, implementation owner/module, stability
channel, authentication, confidentiality, credential/config trust boundary,
extension seam, supported platforms/providers, failure/degradation behavior,
tests, user/operator docs, diagnostics/UI status, and known gaps/issue links.

Required layers are interactive transport; non-interactive control; data/file
projection; Git/forge reads and writes; provider lifecycle/egress/checkpoints/
files; reverse tunnels/Iroh; merge queue; control API; MCP/plugin projection;
credentials; config/trust; UI status; recording; and diagnostics.

## Findings reconciliation

The missing ten findings are recovered from available comments/history or
reconstructed from a fresh code audit. Each finding is marked `resolved`,
`mitigated`, `accepted_risk`, `duplicate`, or `outstanding` with evidence,
rationale, owner, and linked issue where work remains. A count/check ensures
all ten references are accounted for.

## Product-decision matrix

RDP/VNC/Telnet, SFTP/file management, tunnels, host/container management,
fleets, IAM/RBAC, alerts, sharing, recording, serial, tailnet, Proxmox, desktop
sync, CLI, localization, cloud SDKs, and object storage each receive one status:
`Shipped`, `Partial`, `Candidate`, or `Non-goal`. Candidate/Non-goal rows require
product rationale and revisit criteria. Partial/Candidate work links existing
issues first; new issues are created only after the capability is accepted.

## Freshness

The document names an owner and a refresh trigger such as a major transport,
provider, authorization, plugin/control-surface change, or release milestone.
Refreshes update the commit/date and reconcile changed rows/issues rather than
adding an unstructured new appendix.
