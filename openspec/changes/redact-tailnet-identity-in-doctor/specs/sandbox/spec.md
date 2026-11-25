# Sandbox

## MODIFIED Requirements

### Requirement: The tailnet seam probes in thegn doctor

`thegn doctor` SHALL print a `host_discovery`/`tailnet` probe conforming to
the provider-seams probe shape: `Ready` (client present, daemon up, logged in
— notes carry the tailnet/control URL and peer count), `Degraded` (reachable
but nothing advertises Tailscale SSH — plain-sshd fallback named), or
`Unavailable` (binary missing / logged out, with the reason). The probe's
identifying detail — the tailnet name and the control URL — SHALL be redacted
by default, each printed as the shared redaction placeholder, because doctor
output is routinely pasted into issues; when the redacted view withheld
identifying detail, its notes SHALL name `thegn doctor --full` (alias
`--unredacted`) as the flag that prints it, and SHALL NOT claim a redaction
when the row carried no identifying detail. `thegn doctor --full` (alias
`--unredacted`) SHALL print the tailnet name and control URL verbatim,
surfacing headscale deployments. Peer count and Tailscale-SSH availability
SHALL print in both views, with SSH advertisement reported from peer
advertisement and marked unknown when the client version does not expose it.
`thegn doctor bundle` SHALL archive the redacted view.

#### Scenario: Doctor redacts the tailnet identity by default

- **WHEN** `thegn doctor` runs where the tailscale client is present and
  logged in
- **THEN** the `host_discovery`/`tailnet` row's notes carry the client
  version, peer count and Tailscale-SSH advertisement, print the tailnet
  name and control URL as the redaction placeholder, and name `thegn doctor
--full` as the flag that prints the identifying detail

#### Scenario: Full detail is an explicit flag

- **WHEN** `thegn doctor --full` (or `thegn doctor --unredacted`) runs on the
  same host
- **THEN** the row's notes print the tailnet name and control URL verbatim
  and no redaction note is emitted

#### Scenario: No identifying detail means no redaction claim

- **WHEN** the row carries no tailnet name and the control URL is unknown to
  the client
- **THEN** the notes report the control URL as unknown and do not name a
  redaction flag

#### Scenario: Doctor on a headscale-backed client

- **WHEN** `thegn doctor` runs where the tailscale client is logged into a
  headscale `login_server`
- **THEN** the `host_discovery`/`tailnet` row is `Ready` with the peer count
  in its notes and the tailnet name and headscale control URL redacted —
  same seam, same kind, no separate headscale backend; `thegn doctor --full`
  prints both verbatim

#### Scenario: The bundle archives the redacted view

- **WHEN** `thegn doctor bundle` writes its support archive
- **THEN** the extended doctor report inside it uses the redacted identity
  view, and no bundle flag opts into identifying detail
