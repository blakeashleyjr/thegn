# Tasks — WS06, the Windows ladder

Each task is one work item of workstream WS06, named by its plan id; tick it when the
item merges. Phase headings name the wave and the openspec change that carries that
phase's deltas (B6: a change is synced and archived at the end of the wave that
completes it). This change carries the scaffold only; later items attach their deltas
to their own phase's change and cite this one.

## P0: gates, wire foundations, baseline and spikes — this change (W1)

- [ ] WS06-P0-01: OCI on Windows is declined only for local placements (adds this
      change's first MODIFIED requirement)
- [ ] WS06-P0-02: Windows-aware argv truth words
- [ ] WS06-P0-03: `thegn-sandbox-proto` crate with `win_cmdline`
- [x] WS06-P0-04: openspec scaffolding for the Windows ladder (this change;
      `just openspec-validate` is its gate)
- [ ] WS06-P0-05: Windows baseline gate (device and VM; blocks P1-03, P1-04, P2-02,
      P4-04 per B8)

## P1: Job Object limits and truthful reporting (D5 a) — `add-windows-job-limits` (W3)

- [ ] WS06-P1-01: Core lowering of limits, `[sandbox.windows]` config and
      `WinHostFacts`
- [ ] WS06-P1-02a: Helper crate skeleton (`thegn-sandbox-win`)
- [ ] WS06-P1-02b: Job-object FFI (`job` mode)
- [ ] WS06-P1-03: Aggregate job and nested pane job in host
- [ ] WS06-P1-04: `jobobject` argv through the helper; probe; resolution; status
      lifecycle
- [ ] WS06-P1-05: Truth, manifest row, doctor, spec honesty; create the
      `sandbox-windows.md` sections
- [ ] WS06-P1-06: Windows kernel tests for limits (CI + VM; verifies J1-J6)

## P2: WSL2 placement on the argv connector (D5 b) — `add-wsl-connector` (W5)

- [ ] WS06-P2-00: WSL device capture (C1-C11)
- [ ] WS06-P2-01: Pure WSL logic in core
- [ ] WS06-P2-02: Discovery seam and `thegn wsl list`; create `sandbox-windows.md`
- [ ] WS06-P2-03a: Provision and content-addressed Linux thegn install
- [ ] WS06-P2-03b: Post-import scrub and seal-check additions
- [ ] WS06-P2-04: `thegn wsl seal [--check] [--inside]`
- [ ] WS06-P2-05a: Generated WSL argv connector and connector location
- [ ] WS06-P2-05b: In-distro sandbox composition
- [ ] WS06-P2-07: Mode B detection (thegn inside WSL) and doctor notes
- [ ] WS06-P2-08: Retire `Backend::Wsl`
- [ ] WS06-P2-09: Daemon-inside-WSL (Mode A')
- [ ] WS06-P2-10: WSL device verification and evidence (ends the THE-791 WSL refusal)

P2-06 does not exist: keepalive is WS09's `keepalive.hold`, emitted by P2-05a's
generator.

## P3: OCI family on Windows (D5 c) — `add-windows-oci-runtimes` (W4)

- [ ] WS06-P3-01: Windows `Product` rows, fixtures and `npipe://` candidates on WS02's
      planner
- [ ] WS06-P3-02: Path translation and git-dir rewriting for Windows-filesystem
      worktrees
- [ ] WS06-P3-03: Lift the Windows OCI decline per attested row
- [ ] WS06-P3-04: `wslc` spike and Windows engine device capture (O1/O2/O5)

## P4: native isolation via MXC (D5 d) — `add-windows-mxc-backend` (W5)

- [ ] WS06-P4-00: Spike: direct PSEC exports vs pinned `mxc-sdk`; peer identity
      (X1-X3)
- [ ] WS06-P4-01: MXC policy lowering in core
- [ ] WS06-P4-02: Helper `probe` mode and the `WinProbeReport` overlay
- [ ] WS06-P4-03: PSEC FlatBuffer encoder (if the direct path is chosen)
- [ ] WS06-P4-04: Helper `mxc` launch
- [ ] WS06-P4-05: `Backend::WinMxc` wiring, refusals, truth, gating; retire
      `WinAppContainer`
- [ ] WS06-P4-06: Proxy-only network wiring with WS04 (peer identity, held ports)
- [ ] WS06-P4-07: Windows credential deny list as a shared data table
- [ ] WS06-P4-08: MXC device conformance suite, attestation, `verified()`, chain slot
      (ends the THE-791 MXC refusal)

## P5: Windows-host remote specifics (shared with WS09) — `add-windows-ssh-client` (W4)

- [ ] WS06-P5-01: Win32-OpenSSH client branch (no multiplexing)
- [ ] WS06-P5-02: PowerShell and cmd quoting helpers; per-OS ProxyCommand vectors
- [ ] WS06-P5-03: Remote host is Windows: detection and remedy
- [ ] WS06-P5-04: `npipe://` endpoint plumbing

## P6: test infrastructure, distribution, graduation — `add-windows-sandbox-distribution` (W5)

- [ ] WS06-P6-01: Wine coverage for the helper and WSL absence
- [ ] WS06-P6-02: Opt-in Windows CI job additions
- [ ] WS06-P6-03: Local Win11 VM upgrades and the device runner
- [ ] WS06-P6-04: Windows zip contents (B45)
- [ ] WS06-P6-05: Graduate `WinIsolation` (needs P4-08 and P2-10 evidence)
