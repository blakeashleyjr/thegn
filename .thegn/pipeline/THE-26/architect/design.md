# THE-26 — Debugger audit architecture

Status: proposed architecture and coder handoff. This is an audit of the
current branch, not authorization to build a debugger integration.

## Decision summary

thegn already has a substantial self-diagnostics stack: panic reports, an
always-on in-memory WARN ring, opt-in `THEGN_LOG` sinks, an opt-in
`thegn::perf` rollup, the Telemetry LOOP panel, a feature-gated flame-graph
profiler, and a broad `thegn doctor`. The cheap gaps selected for this issue
are:

1. Make the diagnostic bundle include the invoking process's WARN-ring
   snapshot, and harden serialization/copying of crash reports against raw
   sensitive values.
2. Make `doctor --json` explain the built-in BugStalker platform gate, as the
   human report already does.
3. Add a truthful debugging workflow page and correct the sink wording in the
   config example.

These changes are edge improvements. They do not add a wake source, periodic
I/O, a state schema, a config key, an external process provider, a capability
catalog row, or a new control-surface action.

## Evidence-cited matrix

Severity describes user impact or privacy risk if the gap remains; it is not an
estimate of implementation effort.

| Surface                      | What exists on this branch                                                                                                                                                                                                                                                                                                                                                       | Gap / severity                                                                                                                                                                                                                                      | Disposition                                                                                                                                                             |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Panic/crash reports          | `thegn-core` installs the panic path and captures identity, panic text, backtrace, and recent warnings in `crates/thegn-core/src/diagnostics.rs:207-228,230-287`; report files are retained/pruned in `crates/thegn-core/src/diagnostics.rs:289-441`.                                                                                                                            | `CrashReport::render` emits the panic line and ring `original` text without passing them through the canonical redaction seam; **High (privacy)**.                                                                                                  | Chunk 1: redact at serialization and sanitize retained reports when bundling.                                                                                           |
| In-memory WARN ring          | WARN+ events are retained in a bounded, nonblocking ring (`crates/thegn-core/src/diagnostics.rs:130-165`), default capacity 256 (`crates/thegn-core/src/diagnostics.rs:20-25`), and are covered by render tests (`crates/thegn-core/src/diagnostics.rs:506-527`).                                                                                                                | `crates/thegn-host/src/cmd/bundle.rs:33-97` never snapshots the ring. A bundle command is a separate process, so it cannot see a live daemon/host ring; **Medium**.                                                                                 | Chunk 1: add an explicitly named current-process ring entry, including `(none)` when empty. Document the separate-process limitation; do not claim live daemon capture. |
| Debug bundle                 | `thegn doctor bundle` writes extended doctor JSON, redacted config, bounded tails for every configured sink, crash reports, and a manifest (`crates/thegn-host/src/cmd/bundle.rs:33-97`); its local, catalog-gated, admin/operator role is documented at `crates/thegn-host/src/cmd/bundle.rs:1-13`.                                                                             | Crash reports are copied as if already secret-free (`crates/thegn-host/src/cmd/bundle.rs:74-81`), which is false for old or externally-produced retained files. It also has no ring section; **High (privacy) / Medium (diagnostic completeness)**. | Chunk 1 only. Keep the bundle local and bounded; do not persist perf state merely to fill a bundle.                                                                     |
| `THEGN_LOG` sinks and stderr | The core logging contract describes an optional rotating file sink and `THEGN_LOG` filter (`crates/thegn-core/src/log_trace.rs:1-13,207-310`). Host startup installs the ring and optional file sink (`crates/thegn-host/src/run.rs:350-383`); raw fd 2 is separately redirected to `thegn-stderr.log` while the TUI owns the terminal (`crates/thegn-host/src/run.rs:407-431`). | The config example's “stderr is always on” wording can be read as an always-on tracing sink, while host tracing stderr is conditional and raw stderr capture is a separate file; **Low (documentation)**.                                           | Chunk 3: explain the two paths and log locations. No logging policy change.                                                                                             |
| `thegn::perf` rollup         | `crates/thegn-host/src/perf.rs:1-18,22-67,547-603,840-1056` provides opt-in counters/histograms, wake-storm classification, p50/p99 rollups, slow-frame/input warnings, and `THEGN_PERF*` tuning. It is piggybacked on existing wake/render work.                                                                                                                                | Rollups are in-memory/log output only; the bundle has no live perf snapshot or history when logging is off. Adding persistence or a bundle IPC request would add I/O/wake and policy surface; **Medium**.                                           | Record as residual/follow-up. Do not implement in THE-26. Document how to enable the existing rollup in Chunk 3.                                                        |
| Telemetry LOOP overlay       | The Telemetry section includes a `LOOP` subblock with wake/render counts, p99s, hot source, idle state, and a wider graph (`crates/thegn-host/src/panel/sections/telemetry.rs:14-72`). The panel help describes the section (`docs/help/bars.md:129-132`).                                                                                                                       | There is no bundle/export path for the live overlay; **Low/Medium**, but an export protocol is outside a cheap audit gap.                                                                                                                           | Record as residual/follow-up. Explain that it is live-only in Chunk 3.                                                                                                  |
| Flame-graph profiler         | `crates/thegn-host/src/profile.rs:1-10,12-121` implements the feature-gated Unix SIGUSR2 start/stop path and writes timestamped SVGs below `$XDG_STATE_HOME/thegn/profiles`.                                                                                                                                                                                                     | Discoverability and platform/feature limits are not in the debugging workflow; **Low**.                                                                                                                                                             | Chunk 3 documents `just profile`, SIGUSR2, output location, and limits. No profiler redesign.                                                                           |
| `thegn doctor`               | Human doctor output reports installation, runtime, terminal, sinks, crash reports, providers, metrics, and managed tools (`crates/thegn-host/src/cmd/doctor.rs:963-1128,1249+`). The managed-tool report gives BugStalker's unsupported-platform reason (`crates/thegn-host/src/cmd/doctor.rs:2738-2774`).                                                                       | `managed_tools_json` reports name/tier/path/pin/current but omits the platform support/reason (`crates/thegn-host/src/cmd/doctor.rs:2776-2791`); **Medium for automation/support bundles**.                                                         | Chunk 2: mirror the existing pure gate and reason in JSON, with a focused unit test. Do not create adapter rows.                                                        |
| Built-in BugStalker          | The pure core seam is deliberately narrow: pinned `bs` tool, Linux x86-64 gate, and pure launch/attach argv (`crates/thegn-core/src/debug.rs:1-119`). Host actions are setup, path, run, and attach, with platform gating and foreground exec replacement (`crates/thegn-host/src/cmd/debug.rs:1-121`).                                                                          | This is not a general debugger integration. The pin is `0.4.6` (`crates/thegn-core/src/debug.rs:16-25`); changing it is tool-maintenance policy, not a cheap audit fix.                                                                             | Keep behavior unchanged. Document the exact scope in Chunk 3; file any pin review separately if desired.                                                                |
| Debug panel surface          | The panel explicitly reserves `db`/`debug`; it renders “no session”, an empty breakpoints section, and “debugger integration not wired yet” (`docs/help/panel.md:318-327`).                                                                                                                                                                                                      | It is a truthful placeholder, not an integration; **Medium user-expectation gap**.                                                                                                                                                                  | Do not wire it in this issue. Document the placeholder and follow up with a seam sketch.                                                                                |
| Debugging users’ programs    | No DAP client, gdb/lldb pane orchestration, or debugger provider was found. Existing openspec task entries defer DAP run and debugger capabilities (`openspec/changes/audit-debugger-capability/tasks.md:1269-1282`).                                                                                                                                                            | General debugger support is a substantial feature with lifecycle, placement, capability, and security policy; **High scope gap**, not a cheap policy-pure fix.                                                                                      | Follow-up to file; see the provider seam below.                                                                                                                         |

## Existing openspec draft: verified and pruned

The draft files were read before this design: `proposal.md`, `design.md`,
`tasks.md`, and `specs/debugger/spec.md` under
`openspec/changes/audit-debugger-capability/`. The normative current debugger
spec agrees with the code: native BugStalker, managed resolution, the Linux
x86-64 gate, setup/path, and foreground run/attach are already implemented by
the locations cited above.

The draft delta proposing `[[debug.adapters]]`, user-configured lldb/delve
templates, `--adapter`, per-adapter doctor rows, trust gates, and a BugStalker
pin bump is pruned from this issue. It is either an unimplemented debugger
integration or external tool/version policy. It would add a registry, config
surface, completion/control-schema/help obligations, and provider lifecycle
before the audit has established a seam. It therefore fails the cheap,
policy-pure constraint and must not become a chunk. The draft’s broad doctor
claim is only partially true: human output has the built-in tool/platform
diagnostic, while JSON is missing the platform explanation; Chunk 2 closes
that specific evidence-backed gap.

No openspec normative behavior is changed by these artifacts. If a future
debugger feature is approved, it should first update that spec rather than
silently treating the draft as delivered behavior.

## Implementation boundaries and invariants

- Chunk 1 uses the existing `thegn-core` redaction and diagnostics seams. The
  redaction helper must delegate key classification to `redact.rs`/the
  existing `log_redact` rules, preserve safe text, and remain best-effort for
  unstructured panic text. A bare positional secret is not inferable; callers
  must continue to avoid logging it. Serialization is the last safe boundary
  for both new reports and historical reports copied into bundles.
- The bundle’s ring entry must be labeled as the invoking/current process. It
  must not imply access to another process’s memory, add polling, or introduce
  a daemon protocol. Empty data is represented explicitly so the manifest is
  deterministic.
- Chunk 2 must reuse the pure `(os, arch)` BugStalker gate/reason. It must not
  probe a vendor executable, resolve/install tools, add adapters, or create a
  second capability catalog.
- Chunk 3 adds no config key. It registers one help source through the existing
  generated-page path (`crates/thegn-host/src/help/pages.rs:9-63`) and keeps
  the panel placeholder wording truthful. Documentation must distinguish
  tracing sinks, raw stderr capture, in-memory diagnostics, and live-only
  telemetry.
- All work remains substrate-free in `thegn-core`; no vendor-specific logic is
  added there. No new god file is warranted: the selected edits stay in the
  existing focused modules, and the help content is a new page.
- No migration or live-state invocation is authorized. Any manual `thegn`
  command used while implementing or testing must set
  `XDG_STATE_HOME` to a fresh temporary directory.

## Ratchets and catalog impact

There are no new env vars, CLI flags, actions, panels, controls, or config
keys. Therefore the env-overlay, completion-slot, control-schema, help-action,
help-context, and help-panel-prose ratchets should remain unchanged; each
coder must verify that explicitly in its chunk. The existing completion entries
for `debug run`, `debug attach`, and `doctor bundle` already cover the current
CLI. No capability-catalog row is added: a local foreground `bs` exec is not a
new external control surface, and the planned doctor JSON field is diagnostic
data only.

## Follow-ups to file

1. **Debugger provider/DAP capability.** Sketch the seam before implementation:
   an object-safe service/provider interface in `thegn-svc` (or the repo’s
   established service boundary), with a capability set mapped to optional
   operations (`launch`, `attach`, `pause`, `continue`, `step`, breakpoints,
   scopes/variables, and stack frames). Provider implementations own vendor
   protocol/process details at the edge; core remains substrate-free. Each
   provider reports a stable `kind`; `reserved` is retained for future adapter
   kinds and unknown kinds must degrade to unavailable. If these operations are
   exposed through an external/control surface, add them to the one capability
   catalog and its schema ratchet. The seam must account for pane placement,
   cancellation, teardown, trust/permissions, and unsupported capabilities.
2. **Live cross-process diagnostics export.** Define an authenticated,
   bounded request/response seam for daemon/host ring and perf snapshots only
   if support workflows require it. Preserve 0% idle and avoid an implicit
   polling loop; never make the local bundle pretend its own ring is the live
   daemon ring.
3. **BugStalker pin maintenance.** Independently verify the managed-tool pin,
   platform availability, and supply-chain policy before changing `BS_PIN`.
4. **Perf bundle history.** If requested, design persistence and retention as
   an explicit policy/config change; it is not justified by this audit alone.

## Chunk map

The three chunks below are independent and file-disjoint, so the Lead may
parallelize them. Each coder owns the exact files listed in its chunk and must
use the exact commit subject specified there.
