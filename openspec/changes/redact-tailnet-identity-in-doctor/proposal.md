# Redact tailnet identity in doctor

## Why

Unfiltered `thegn doctor` output includes the host's tailnet name and the
tailnet control URL. Doctor output is routinely pasted into issues, so those
two strings — which identify the user and (for headscale deployments) their
self-hosted control server — leak by default into support threads.

Found while recording demo media (THE-2682).

## What Changes

- The `host_discovery`/`tailnet` probe's notes redact the tailnet name and
  the control URL by default, replacing each with the shared
  `thegn_core::redact::PLACEHOLDER` (`***redacted***`). Everything a triager
  needs stays: client version, peer count, Tailscale-SSH advertisement
  counts, backend-state reasons.
- The redacted row names the reveal: a note on the row says that `thegn
doctor --full` (alias `--unredacted`) prints the identifying detail, so the
  opt-in is discoverable rather than hidden. The note appears only when
  something was actually redacted.
- `thegn doctor --full` (alias `--unredacted`) opts into verbatim identity,
  in both the text report and `--json`. The flag is documented in `--help`.
- `thegn doctor bundle` always archives the redacted view: the bundle is a
  support archive written to be shared, so there is no bundle-side opt-in.
- Redaction lives in the pure model (`thegn_core::tailnet::probe_summary`
  gains an `IdentityDetail` argument) and is threaded explicitly through the
  seam (`HostDiscovery::probe_with`) and the registry
  (`seam::registry::probes_with_detail`); the no-argument `probes`/`probe`
  entry points default to the redacted view, so surfaces that print probes
  without a user opt-in cannot leak the identity. `thegn host discover`
  (which the user runs on purpose, on their own machine, in their own
  terminal) is unchanged.

## Impact

- **Specs:** `sandbox` — "The tailnet seam probes in thegn doctor" is
  modified: the verbatim-control-URL mandate becomes redact-by-default with an
  explicit `--full`/`--unredacted` opt-in, and the bundle stays redacted.
- **Code:** `thegn-core/src/tailnet.rs` (redaction in the pure model),
  `thegn-svc/src/host_discovery/mod.rs` (seam threading),
  `thegn-svc/src/seam/registry.rs` (registry default + explicit variant),
  `thegn-host/src/main.rs` (the flag), `thegn-host/src/cmd/doctor.rs`
  (threading into the text/JSON/bundle reports).
- **Docs:** `docs/help/debugging.md`, `docs/help/privacy.md`,
  `docs/help/cli.md` — the default, the flag, and the bundle behavior.
- **Proof:** hermetic unit tests pin the redacted rendering (including a
  headscale capture), the registry default, the no-identity no-claim case,
  and the doctor threading; `doctor bundle` keeps the redacted view by
  construction (it calls the unchanged `doctor_json_with_health`). Execution
  on a host with a live tailscale client is left for review: this worktree
  forbids local builds.
