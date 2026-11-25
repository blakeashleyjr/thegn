# Design

The tailnet identity is exactly two strings in the probe's notes: the
`CurrentTailnet.Name` value ("tailnet: …") and the control URL ("control
URL: …"). Both identify the user; for a headscale login the control URL also
names their self-hosted server. Everything else in the row — client version,
peer count, Tailscale-SSH advertisement counts, backend-state reasons — is
triage signal with no identity, so it stays unredacted.

Redaction happens in the pure model, not at the printing edge: `probe_summary`
takes an `IdentityDetail` argument (`Redacted` | `Full`) and rewrites the two
identifying notes when it is `Redacted`, substituting
`thegn_core::redact::PLACEHOLDER` rather than a local string, so every leak
surface shares one placeholder. Availability and note count never depend on
the detail: the redacted view keeps the provider-seams probe shape intact
(same row, same order), which keeps the registry deterministic and the
conformance invariants true in both views.

The opt-in is discoverable, not hidden: when the redacted view actually
withheld something, it appends one note naming `thegn doctor --full` (alias
`--unredacted`) as the flag that prints what was withheld. A row that carried
no identity (no tailnet name, control URL unknown) appends nothing, so the
note never claims a redaction that did not happen.

Threading is explicit end to end so the default is structural, not
conventional:

- `HostDiscovery::probe_with(IdentityDetail) -> ProbeReport` is the required
  method; the trait's provided `probe()` defaults to the redacted view, so a
  caller that prints probes without a user opt-in cannot leak the identity.
- `seam::registry::probes(cfg)` keeps its signature and delegates to
  `probes_with_detail(cfg, IdentityDetail::Redacted)`; `probes_with_detail`
  is the explicit variant `thegn doctor` calls with the flag's choice.
- `cmd::doctor::run` maps `--full` to `IdentityDetail::Full` and threads it
  into both `providers_report` (text) and `doctor_json_with_health_and_overrides`
  (JSON). `doctor bundle` calls the unchanged `doctor_json_with_health`,
  which pins the redacted view: the bundle is written to be shared, so it has
  no opt-in, mirroring the issue's "redact by default" intent for the archive
  a user is explicitly told to attach.

`thegn host discover` is deliberately untouched: it is an explicit
user action listing the user's own peers in their own terminal, not a
paste-into-an-issue surface, and the issue scopes the change to `doctor`.
