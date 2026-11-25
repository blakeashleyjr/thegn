# Tasks

- [x] Read the CLI-subcommand extension recipe, the provider-seams spec, and
      the sandbox spec's tailnet-probe requirement; trace every caller of
      `probe_summary`, `HostDiscovery::probe`, `registry::probes`,
      `providers_json`, `providers_report`, and the `doctor_json*` family.
- [x] Add `IdentityDetail` to `thegn_core::tailnet` and make `probe_summary`
      redact the tailnet name and control URL (shared
      `thegn_core::redact::PLACEHOLDER`) in the redacted view, appending the
      `--full`/`--unredacted` reveal note only when something was withheld.
- [x] Thread the detail through the seam: `HostDiscovery::probe_with` with a
      provided `probe()` defaulting to redacted; `registry::probes_with_detail`
      with `probes` delegating to the redacted view.
- [x] Add the `thegn doctor --full` flag (clap alias `--unredacted`) and
      document it in the `Doctor` command help; thread it through dispatch
      into `cmd::doctor::run` and both the text and JSON reports; keep
      `doctor bundle` on the redacted view.
- [x] Hermetic unit tests: redacted/full rendering from the existing
      tailscale and headscale status fixtures (including the no-identity
      no-claim case), the registry default delegation, the seam default, and
      the doctor JSON threading (shape-stable across detail with a missing
      client).
- [x] Update `docs/help/debugging.md`, `docs/help/privacy.md`, and
      `docs/help/cli.md` for the default, the flag, and the bundle behavior;
      no completion-slot ratchet line is needed (the flags are boolean).
- [x] Modify the sandbox spec requirement in this change's delta and index
      the change in `delivery/index.json` with the public-launch
      untracked-rationale.
- [x] Build and run the focused tests on a host that permits it (this
      worktree forbids local builds); rerun `test/smoke.sh` doctor checks
      with a live tailscale client to eyeball the redacted row and `--full`. Covered by the alpha.134 Mac gate (23739 tests, 5/5) on 2026-11-25.
