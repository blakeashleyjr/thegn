# Validation receipts and acceptance coverage

Date: 2026-10-13. Source revision:
`38e09ded78652c6cf56b0e3a7b6a7c16d18dcd95`.

The audit ran in an independent clone and three worktrees of that clone. The
shared `/home/blake/code/thegn` checkout was only read for initial status and
clone input; no worktree metadata, build output, fixture or report was written
there. No application code/API was changed. Subagent evidence commits were
cherry-picked only into the independent audit checkout.

## Safety and reproducibility

Runtime fixtures use an empty synthetic HOME, owned repo/linked-worktree/sibling
directories, fake credential contents and generated cache/system-path canaries.
The Git experiment fixtures disable system/global config; runtime setup uses
synthetic Git identity/configuration and newly initialized repositories. Probe
processes get an explicitly selected environment; no real credential content
is used as a canary. Only controlled local TCP and UDP responders receive probe
traffic. Podman uses a cached image ID with no pulls and unique audit-owned
container names. No other agents' containers or state are changed. Existing
DNS tests that send requests to external upstreams were not run unmodified.
Vendor documentation research is separate from execution probe traffic.

To repeat the lightweight evidence from the audit checkout:

```sh
python3 docs/audits/sandbox-security-2026-10-13/backend-probe.py
python3 docs/audits/sandbox-security-2026-10-13/private-copy-experiments.py
python3 docs/audits/sandbox-security-2026-10-13/host-git-experiments.py
```

The first fixture compiles extracted helper logic from the checked-out source
with one disclosed logging-macro replacement; it uses a localhost DNS upstream.
It reproduces source-set reuse, extra removed mounts, first-policy singleton
reuse and DNS case matching. It passed twice in the backend worktree and again
in the root audit checkout. The second fixture runs 18 checks under a new
TemporaryDirectory and empty Git config, cleans it up, and prints a JSON receipt
matching [private-copy-results.json](private-copy-results.json). It passed in
the root audit checkout. It tests Git primitives and recovery predicates, not
an implemented Thegn sync engine.

The third fixture repeats the synthetic fsmonitor/status and post-commit checks
with markers outside its repo but inside an owned temporary directory. Both
passed in the root checkout; [host-git-rerun.json](host-git-rerun.json) retains
the receipt in addition to the subagent's original [receipt](host-git-canary.json).
Only synthetic local Git helper scripts execute.

The compiled runtime helper, driver commands, logs, environment restrictions,
cached image, and supported/blocked outcomes are indexed in
[runtime-evidence.md](runtime-evidence.md). Its standalone Cargo package is
an audit fixture under `runtime/`, not a workspace member or product API.
Offline dependency resolution and an independent target directory avoid
modifying the shared checkout or fetching new dependencies. Re-running runtime
probes should follow the evidence's local-image and owned-resource prerequisites.
Do not turn skipped upstream integration tests into passed runtime claims.

## Executed audit scenarios

| Scenario from the plan | What was actually checked | Scope limit |
| --- | --- | --- |
| Filesystem reads/writes against home/sibling/common Git/cache/credential/system paths | Actual core-generated bwrap commands with synthetic home and canaries; Podman commands with minimal explicit fixture mounts; systemd attempts; literal/default vs supported GPG mount grammar. | OCI minimal config is deliberately distinct from default NixOS auto-substrate. All launcher UIs and other OSes were not executed. See raw runtime logs for successful/blocked rows. |
| Network domain lookup, direct IP and alternate DNS | Production DNS singleton with controlled UDP upstream; local TCP and UDP probes inside actual bwrap/Podman; fresh none vs retained NAT→none; OCI DNS option acceptance. | No external endpoint, DoH server, real VPN or independent Docker daemon. IPv6/raw-socket and platform-specific bypass testing remains a later gate. |
| Global/repo policy | Actual `Config::resolve_env` with sealed global ceiling, checked-in weaker environment selection and deny-all approvals. Source audits cover trust classification/allow/block intersection and queue policy omission. | This is actual resolver behavior, not an interactive end-to-end spawn. Broad Cartesian policy combinations are not certified. |
| Secure fallback/refusal | Actual core floor decisions and missing-backend resolution; source traces final interactive admission, terminal/queue/provider exceptions, sealed home/Compose gates. | Compiled host UI unavailable in this focused audit. Core `resolve` does not itself supply final host admission; a resolver fallback result is not proof of a secure UI launch. |
| Host Git executable paths | Direct synthetic Git status fsmonitor and post-commit marker checks using production local argv/env shapes. | Thegn's compiled UI/status loop was not invoked; source trace establishes host routing. No real Git hook executed. |
| Independent private repository | `.git` dir/common-dir confinement, no alternates/shared object inodes, dirty and untracked capture, private commits/refs leave host refs untouched. | Synthetic owned repos only. Thegn private-copy mode does not exist yet. |
| Explicit file/patch sync | Synthetic tracked modification, untracked deletion/new additions, binary patch; host index/refs/pointer untouched; ignored file remains excluded/unchanged. | `git apply` is used only with known synthetic fixture paths. It is not a hostile-path exporter or production transaction implementation. |
| Host changes since baseline | Content-baseline conflict predicate detects changed host bytes without overwriting either side. Repeated successful baseline comparison gives no pending delta. | No concurrent-writer stress or compare-and-swap guarantee. |
| Restart and interrupted sync | A new process reads durable private files/manifest; recovery predicates distinguish old/new/third-party bytes. | No implemented application/runtime persistence or journal exists to kill/restart. These are design feasibility checks, not crash-consistency validation. |

Focused existing sandbox-profile/read-only-home/sealed-home tests were also run
offline by the runtime agent. Its report retains counts and the failed shared
test-process runs. Sealed-home tests mutate HOME and document process isolation;
individual exact-test runs with synthetic supported HOME separate fixture
assumptions from runtime guarantees. This is not a clean full-workspace test
claim, and no production fixes were made to change results.

## Later implementation release gates

The design requires actual acceptance for each scenario below. These checks are
**not passed by this milestone** because private-copy/sync and a unified secure
runtime adapter have not been implemented.

| Gate | Required result |
| --- | --- |
| Every launcher and supported backend | Same effective filesystem/credential/egress policy or explicit refusal, including tests, gates, services and native providers. Secure failure never spawns host code. |
| Filesystem scope and linked-Git attacks | Home/sibling/host common Git/broker metadata denied; private worktree writes allowed; only user-reviewed cache/credential grants; unsafe custom mounts rejected. |
| Packet/connection denial | Direct IP, alternate UDP/TCP DNS, DoH/DoT, IPv6/mapped addresses, raw sockets, proxy-env removal, LAN/metadata/host bridge probes denied unless explicitly permitted. |
| Policy precedence and reuse | Global ceiling survives repo/env/provider changes; repo restrictions narrow; per-repo widening is user-owned; live policy matches admission generation. |
| Persistent storage | Runtime/container recreation, application/gateway kill/restart, disconnect and dormancy preserve private repo and immutable baseline; compute loss never discards files or silently recaptures host. |
| File transfer | Dirty/staged/tracked/untracked/binary/mode/delete/rename edits, ignored inputs, hostile filenames, symlinks and archive attacks handled; no host `.git`, refs/history or executable config transferred. |
| Conflicts | Host bytes/type/mode/ancestry/HEAD/index movement detected; whole transaction refused before intentional writes; concurrent unexpected bytes retained on detected race/recovery. |
| Sync failpoints | Kill after every prepare/journal/write/rename/fsync/baseline receipt step; recover only old/new states; retain conflicts/backups; repeated transaction receipt idempotent. |
| Discard and cleanup custody | No automatic prune of private data; explicit discard revokes creds/compute and deletes only verified owned workspace resources; pending sync prevents silent deletion. |
| Observability | Persistent redacted requested/resolved/observed policy and denial/sync events, correct ownership labels, per-runtime/workspace IDs, event-loss counts and restart readback. |

The completed milestone is a source/runtime audit with a decision-ready design.
Outstanding release gates are limits on future product implementation, not
unfinished code changes in this audit.
