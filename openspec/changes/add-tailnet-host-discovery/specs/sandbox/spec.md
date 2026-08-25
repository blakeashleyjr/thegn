# Sandbox

## ADDED Requirements

### Requirement: Tailnet host discovery is a provider seam

thegn SHALL discover remote-host candidates from the user's mesh VPN through a
`host_discovery` provider seam (object-safe trait, config `kind`
implemented-or-`reserved`, errors classified per `thegn_core::seam`), whose
first implemented kind is `tailnet`: enumerate peer devices from the **local**
tailscale client (`tailscale status --json`), yielding for each candidate its
MagicDNS name, OS, online state, tags, stable node id, and whether Tailscale
SSH is advertised. Parsing MUST be pure `thegn-core` logic (unit-tested);
the subprocess is the I/O seam. Discovery MUST run only on explicit user
action (CLI verb, wizard, palette), off the event loop with results delivered
over a channel that pulses the `TerminalWaker` — never a background scan or a
polling timer. Candidates MUST NOT be persisted; the tailnet is the source of
truth. A headscale control plane SHALL be served by the same seam and kind
(the local client is control-plane-agnostic); control-plane differences are
reported by the probe, never assumed.

#### Scenario: Discovering candidates from a logged-in tailnet

- **WHEN** the user runs `thegn host discover` on a machine whose tailscale
  client is logged in (to Tailscale or a headscale `login_server`)
- **THEN** thegn lists the tailnet's peer devices as host candidates with
  name, OS, online state, tags, and SSH advertisement — prompting for no
  credential and writing nothing to config or the DB

#### Scenario: Missing client degrades cleanly

- **WHEN** `tailscale` is not on PATH (or tailscaled is not running)
- **THEN** discovery fails with a classified error naming what is missing
  (`NotInstalled` / `Transient`), with no panic and no partial candidate list

### Requirement: Discovered hosts connect credential-free over Tailscale SSH

Promoting a tailnet candidate SHALL be an explicit user action that produces a
`[host.<name>]`-shaped entry with `reach = "ssh"` and an `SshTarget` of the
MagicDNS FQDN on port 22 carrying **no identity file, password, token, or any
stored secret**: when the target runs Tailscale SSH, authentication and
authorization are delegated to tailscaled and the tailnet's ACLs; a target
running plain sshd over the tailnet rides the identical target through the
user's own ssh agent/config. Promotion MUST write global config only (repo
overlays cannot define hosts), MUST leave `install_runtime` consent at its
default (`ask`), and connect MUST honor ssh host-key verification (never
disabling strict checking). An ACL or sshd refusal SHALL surface as a
classified `Auth` error with no credential prompt and no retry storm.

#### Scenario: Promote and connect without stored credentials

- **WHEN** the user promotes candidate `nuc.tail1234.ts.net` and opens a
  worktree pane on it
- **THEN** the pane's connect argv references the MagicDNS name with no `-i`,
  no password, and no thegn-minted key, and the session succeeds iff the
  tailnet ACLs (or the host's sshd) allow it

#### Scenario: ACL denial is surfaced, not retried

- **WHEN** the tailnet ACLs deny ssh from this device to the promoted host
- **THEN** the failure surfaces as an auth-classified error naming the host,
  and thegn neither prompts for a credential nor loops reconnecting

### Requirement: The tailnet seam probes in thegn doctor

`thegn doctor` SHALL print a `host_discovery`/`tailnet` probe conforming to
the provider-seams probe shape: `Ready` (client present, daemon up, logged in
— notes carry the tailnet/control URL and peer count), `Degraded` (reachable
but nothing advertises Tailscale SSH — plain-sshd fallback named), or
`Unavailable` (binary missing / logged out, with the reason). The probe MUST
report the control URL verbatim (surfacing headscale deployments) and MUST
report Tailscale-SSH availability from peer advertisement, marking it unknown
when the client version does not expose it.

#### Scenario: Doctor on a headscale-backed client

- **WHEN** `thegn doctor` runs where the tailscale client is logged into a
  headscale `login_server`
- **THEN** the `host_discovery`/`tailnet` row is `Ready` with the headscale
  control URL and peer count in its notes — same seam, same kind, no separate
  headscale backend

### Requirement: Host discovery is a capability-catalog row

The discovery verb SHALL exist as one `thegn_core::capability::CATALOG` row
(`host.discover`, read scope) projected to its surfaces (CLI at minimum),
gated by `required_scope(verb)` — never a second policy table — and its list
output SHALL support `--json`.

#### Scenario: The verb is catalog-routed

- **WHEN** the capability catalog is enumerated (`thegn api list`)
- **THEN** `host.discover` appears with its surfaces and required scope, and
  the CLI verb resolves through the same catalog row
