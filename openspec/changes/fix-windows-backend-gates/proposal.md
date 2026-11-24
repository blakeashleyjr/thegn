# Scaffold the Windows backend gate for the WS06 ladder

## Why

The Windows workstream (WS06) is 46 items delivered in D5's order, but it exists
today as six in-flight `add-windows-*` changes, each describing one slice of the
port: `add-windows-native-compile` (20/23 tasks),
`add-windows-compositor-validation` (7/17, the on-machine checklist unchecked),
`add-windows-ci-distribution` (8/11), `add-windows-daemon-ipc` (10/11),
`add-windows-job-objects` (10/11) and `add-windows-parity` (9/10).
`openspec/specs/platform-windows/spec.md` records what the port already promises.
None of it states the ladder order, the evidence-tag policy, which item verifies
each unverified runtime claim, or when a Windows row may be called `Verified`.

That gap is a live risk, not a tidiness one. Most Windows runtime behaviour is known
from vendor documentation or background knowledge, and rule B8 forbids merging an
item whose design rests on such a fact before its spike's decision record exists.
Without one register, a later item can build on a `[kb]` claim that no item will
check, or flip a backend row to `Verified` on a hosted-runner run that proves nothing
about the kernel the feature needs.

This change is the scaffold for that record. It states the ladder (D5), the
evidence-tag policy and B8, the register of every unverified Windows claim with the
item that verifies it, and the `verified()` evidence rule. Later WS06 items attach
their deltas to it; it is synced and archived at the end of W1 once the WS06 P0 items
land (B6).

Nothing here lifts a refusal. THE-791 (T8, gate THE-795) refuses native Windows
(Job Object) containment for a secure policy always, WSL placement with an inner
Linux backend only after WS06-P2-10 evidence, and MXC only after WS06-P4-08 evidence.
This change records that the ladder builds those backends because their evidence
items end the refusal, and no requirement or sentence here removes or weakens a
refusal for any secure policy.

## What Changes

- New change `fix-windows-backend-gates`, the scaffold for workstream WS06: this
  proposal, a `tasks.md` ledger of the WS06 items by phase, and one ADDED requirement
  in the `platform-windows` delta.
- No code, config, CLI, capability or user-visible behaviour changes. This item is
  documentation and openspec only.

The proposal records, in the sections below: the D5 ladder (section 1), the
evidence-tag policy and rule B8 (section 2), the evidence register (section 3), the
`verified()` rule (section 4), and how this change relates to the six open
`add-windows-*` changes and to the roadmap index (section 5).

### 1. Decision D5 — the ladder order

All Windows backend work SHALL proceed in this order, and each step SHALL rest on the
evidence of the step before it:

- **(a) Job Object limits with truthful reporting.** `jobobject` gains a per-pane CPU
  hard cap, a per-pane commit-memory cap, an active-process cap and UI restrictions,
  with an aggregate CPU cap across panes nested under the existing kill-on-close pane
  job. Limits are derived from the argv, so the "containment invisible to argv" spec
  exception is removed for Windows.
- **(b) WSL2 as a placement.** A generated WS09 argv connector
  (`kind = "argv"`, `path_style = "wsl"`, `target_os = "linux"`) runs the distro's own
  thegn, which composes the Linux sandbox (bwrap/Landlock/podman argv) inside the
  distro. The Windows thegn never composes a Linux sandbox argv.
- **(c) The OCI family on Windows.** Containers inside the WSL placement come first;
  native Docker Desktop, podman machine and `wslc` against Windows-filesystem
  worktrees stay behind an opt-in, with path translation and git-dir rewriting.
- **(d) Native isolation through a small separate helper.** The helper uses Microsoft
  MXC's process security environment (PSEC), is supported on Windows 11 24H2 or later,
  and fails closed on anything else. It is never linked into `thegn-host` or
  `thegn-core`.

### 2. Evidence tags and rule B8

Every external fact a WS06 item rests on carries one tag:

- **`[code]`** — re-read in this repository.
- **`[src]`** — read in a vendor source clone.
- **`[doc]`** — vendor documentation.
- **`[kb]`** — background knowledge, not verified.
- **`[device]`** — needs a real Windows machine or the Win11 KVM VM.

Every `[kb]` or `[device]` claim MUST name the spike or on-device item that verifies
it. Rule B8 then binds the merge: no item whose design rests on an unverified external
fact merges before its spike's decision record exists. A spike is a `P0`-phase item or
carries an `s` suffix, the dependent item lists it as a dependency, and a check that
needs a device is recorded in the PR with the host and date.

### 3. Evidence register

Every Windows runtime claim, the tag it carries today, the item that verifies it, and
the items that may not merge before that decision record exists:

| Claim                                                                                    | Tag now                          | Verified by             | No merge before the decision record                                          |
| ---------------------------------------------------------------------------------------- | -------------------------------- | ----------------------- | ---------------------------------------------------------------------------- |
| Native port builds, runs, renders, Ctrl+C, daemon race                                   | \[device\] unknown (never green) | WS06-P0-05              | P1-03, P1-04, P2-02, P4-04                                                   |
| Job CPU rate, commit cap, active-process cap, UI limits, nesting under the aggregate job | \[kb\]                           | P1-06, J1-J5            | P1-05 doc wording ("pending on-device validation"); `jobobject` verification |
| Console handler not inherited; Ctrl+C once                                               | \[kb\]                           | P1-06, J6               | P1-05 docs                                                                   |
| `wsl -l -v` encoding, Lxss layout, policy key                                            | \[kb\]                           | P2-00 C1/C2/C9, W11     | P2-01, P2-02                                                                 |
| `--cd`/`-e`/`-u`                                                                         | \[doc\]                          | P2-00 C3, W4            | none                                                                         |
| ConPTY -> wsl.exe -> Linux pty, resize, Ctrl+C                                           | \[kb\]                           | P2-00 C3/C4, W8         | P2-01, P2-02, P2-05a                                                         |
| Held `sleep infinity` counts as activity                                                 | \[kb\]                           | P2-00 C6 (+ WS09-P0-03) | P2-05a (`hold` emitted or not)                                               |
| fd 3 does not cross wsl.exe                                                              | \[kb\]                           | P2-00 C7                | P2-05b (`--intent-b64`)                                                      |
| `WSLENV` with interop disabled                                                           | \[kb\]                           | P2-00 C8                | P2-05b env transport                                                         |
| `wsl --export -` / `--import -` streaming                                                | \[kb\]                           | P2-00 C10               | P2-03a                                                                       |
| `wsl.exe -e` stdio binary-safe                                                           | \[kb\]                           | WS09-P0-03              | P2-03a transfer, P2-05a `exec_binary_safe`                                   |
| WSL kernel Landlock ABI <= 3, userns on, AppArmor absent                                 | \[kb\]                           | P2-00 C5, W5            | P2-05b docs                                                                  |
| Sealing keys effective; WSLInterop absent                                                | \[doc\]/\[kb\]                   | W3                      | P2-04 docs                                                                   |
| Docker Desktop / podman machine pipe names and JSON                                      | \[kb\]                           | P3-04 (O1/O2)           | P3-03 row flips                                                              |
| `wslc` CLI surface; `container.exe` alias                                                | \[doc\]/\[kb\]                   | P3-04 (O5)              | WS02 `wslc` row; alias stays defensive only                                  |
| PSEC exports, build table, create/close probe, FS_DENY                                   | \[src\]                          | P4-00 X1, P4-08 R1-R3   | P4-01..P4-05                                                                 |
| `allowed_appcontainer_peer` string and identity-scoped loopback closure                  | \[src\]                          | P4-00 X2, N4            | P4-06 `Enforce`; P6-05                                                       |
| `ProxyInfo.url` userinfo                                                                 | \[kb\]                           | P4-00 X3                | P4-06 token placement                                                        |
| ConPTY inheritance under PSEC; console mode handles                                      | \[src\]                          | P4-00 X1, P1            | P4-04                                                                        |
| AppContainer cannot open the daemon pipe                                                 | \[kb\]                           | F6                      | section 4 row                                                                |
| AppContainer children cannot create child AppContainers                                  | \[src, unverified\]              | P7                      | WS11 MXC nesting preset rows                                                 |
| Win32-OpenSSH no ControlMaster; `ssh -V` string                                          | \[doc\]/\[kb\]                   | WS09-P0-03, R4          | P5-01                                                                        |
| Hosted runners lack nested virtualisation                                                | \[kb\]                           | P6-02 spike             | CI WSL smoke                                                                 |

The check ids (J, C, W, O, F, N, P, R) name entries of the WS06 device checklists; a
run is recorded in the owning change's `evidence/` directory, which the first run
creates.

### 4. The `verified()` rule

Minimum evidence before any Windows row becomes `Verified` or `Attested`:

1. A committed evidence file naming the device, the OS build and UBR, the thegn commit
   and the helper version, plus an attestation row in `sandbox_attestations`
   (WS02-P5-03b).
2. All mandatory checks pass: `jobobject` J1-J3 and J5-J7; `mxc` F1-F4, N1-N2, N4
   decided, P1-P3 and R1-R2; the WSL connector recipe `live-tested` W1-W6 and W8-W10;
   each Windows OCI row its O checks plus its `--cpus`/`--memory` limit checks.
3. An opt-in CI run is green for everything CI can run.
4. The doctor output in the evidence matches the manifest row.

Without hardware, rows stay unverified and `WinIsolation` stays Experimental;
`jobobject` limits ship with "pending on-device validation" in doctor.

### 5. Relation to the six open `add-windows-*` changes, and the roadmap index

The six open Windows changes stay as they are; this change neither amends nor
supersedes them:

- `add-windows-native-compile` (phase 1: the workspace compiles for Windows, platform
  seams, CI cross-check) — 20/23 tasks.
- `add-windows-daemon-ipc` (phase 2: named-pipe daemon IPC) — 10/11 tasks.
- `add-windows-job-objects` (phase 3: kill-on-close Job Objects) — 10/11 tasks.
- `add-windows-compositor-validation` (phase 4: Windows Terminal compositor, the
  on-machine checklist) — 7/17 tasks; WS06-P0-05 checks it off and later archives it.
- `add-windows-parity` (phase 5: activity, fsperm, backend policy, toasts) — 9/10.
- `add-windows-ci-distribution` (phase 6: CI promotion and the Windows zip) — 8/11.

Those are the port record. This change is the gate record for the work built on top of
it, and it owns one new `platform-windows` requirement (the ladder and the evidence
rule) that none of the six touches. WS06-P0-01 adds the first MODIFIED requirement to
this same delta file ("decline is local"); later phase changes
(`add-windows-job-limits` W3, `add-wsl-connector` W5, `add-windows-oci-runtimes` W4,
`add-windows-mxc-backend` W5, `add-windows-ssh-client` W4,
`add-windows-sandbox-distribution` W5) are created by their own items and attach their
deltas to their own change, citing this one.

Roadmap index (`tasks.md`): this programme cites group **BB** (sandbox + remote
programme), item 763, whose row points at
`docs/superpowers/plans/2026-11-17-sandbox-remote-programme.md`; and group **AX**
(native Windows support), items 728a-737, which is the Windows port the ladder builds
on. The programme item is WS06-P0-04. `tasks.md` in this change lists every WS06 item
by id under its phase.

## Impact

- **Roadmap:** `tasks.md` group BB item 763; group AX items 728a-737; programme item
  WS06-P0-04 (wave W0).
- **Specs:** one ADDED requirement in `platform-windows`; no existing requirement is
  MODIFIED or REMOVED. The ADDED requirement is this change's own content — the
  ladder order plus the `verified()` gate — and is what makes the delta non-empty for
  `openspec validate --strict`; WS06-P0-01 adds the first MODIFIED requirement to the
  same delta file.
- **Spec collision:** none. No other in-flight change modifies or removes a
  `platform-windows` requirement this change adds (B6).
- **Code, config, CLI, doctor, help:** unchanged. No ratchet moves.
- **Archive:** at the end of wave W1, once the WS06 P0 items land and their deltas
  have been folded in (B6). Later WS06 items hold their deltas in their own changes.
