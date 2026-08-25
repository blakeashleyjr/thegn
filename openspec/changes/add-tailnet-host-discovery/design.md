# Design

## Context

Two tailscale integrations exist and stay untouched: the sandbox egress
sidecar (`crates/thegn-svc/src/vpn/mod.rs` — joins a _worktree sandbox_ to a
tailnet with its own minted identity) and the share seam (ingress via
`tailscale serve`). This change is the third, missing direction: **thegn's own
machine is already on a tailnet; use that membership to find and reach the
user's other machines** as remote-host candidates for the existing
`[host.<name>]` / `SshTarget` stack.

## Seam design

```text
thegn-core (pure, 95% gate)          thegn-svc (I/O seam)         thegn-host
  tailnet.rs                           host_discovery/mod.rs        cmd/host.rs discover
  HostCandidate { name, fqdn,          trait HostDiscovery:         wizard candidate step
    os, online, tags,                    kind() -> &'static str     doctor probe row
    ssh_advertised, node_id }            discover() -> BoxFuture<   palette entry
  parse_status_json(&str)                  Result<Vec<HostCandidate>>>
  filter(candidates, &cfg)               probe() -> ProbeReport
```

- **Trait shape** follows `thegn_core::seam`: object-safe (`BoxFuture`), errors
  carry `ErrorClass` (`NotInstalled` when the binary is absent,
  `NotConfigured` when logged out, `Transient` when tailscaled is unreachable),
  `kind` values `tailnet` (implemented) and `mdns`/`consul` (`reserved` — the
  config enum rejects them with the standard reserved message).
- **Discovery source**: `tailscale status --json` via the configured
  `tailscale_bin` (default `tailscale`). The JSON's `Peer` map provides
  `DNSName`, `OS`, `Online`, `Tags`, `ID`/`PublicKey`, and `sshHostKeys`/
  `SSH_HostKeys` presence (⇒ Tailscale SSH advertised). Parsing is pure in
  `thegn_core::tailnet` with fixture-based unit tests (real captured JSON,
  including a headscale-backed capture). The LocalAPI unix socket
  (`/var/run/tailscale/tailscaled.sock`) is a documented later layer behind the
  same trait — not in scope, noted `reserved` in the probe notes rather than a
  half-implemented path.
- **Headscale**: no second kind. The local client behaves identically against
  a headscale `login_server`; `tailscale status --json` self-describes the
  control URL (`ControlURL` in `tailscale debug prefs` / status header fields
  where available). The probe reports the control URL verbatim and reports
  Tailscale-SSH per-peer from advertisement, never from an assumption about
  the control plane.

## Connect path

Promotion produces exactly what the existing stack consumes:

- `SshTarget::plain(fqdn, 22, false)` — **no** `identity`, **no** `ssh_config`,
  no password, no token. With Tailscale SSH on the target, `ssh <fqdn>` is
  intercepted by tailscaled; authn/authz are tailnet-ACL decisions. With plain
  sshd over the tailnet, the user's normal ssh agent/config applies — same
  argv, no thegn-stored credential either way.
- Everything downstream is existing machinery: `GitLoc` control reads over
  ControlMaster, mosh/ssh interactive panes (`SshPlacement`), and — once
  `add-host-as-resource` lands — the host state machine's `connect → probe`
  steps.
- Host-key trust: first connect goes through the normal ssh known-hosts flow
  (or, for host-flow, the per-instance known-hosts registry the VPS ssh-shim
  established). The candidate carries the tailnet's stable `node_id` so the
  wizard can display it; thegn does not invent its own pinning layer.

## Runtime shape (event-loop rules)

- Discovery is **on-demand only** (CLI verb, wizard step, palette action) and
  runs on a worker (`spawn_blocking`-class, QoS `Utility`); results return over
  a channel and **pulse the `TerminalWaker`**. No background rescans, no
  polling timer — the 0%-idle contract is untouched.
- **Render damage**: the wizard/palette candidate list is chrome ⇒ `Full`
  frames on open/update, via the existing overlay path. No new damage channel;
  pane output is unaffected.
- **SQLite**: no schema change, no `user_version` bump. Candidates are
  ephemeral query results.

## Surfaces and gates

- `host.discover` is one `thegn_core::capability::CATALOG` row
  (`required_scope`: read), projected to the CLI (`thegn host discover
[--json]`) per the capability recipe (`docs/extending/capability.md`,
  `cli-subcommand.md`); the catalog-coverage test is the gate.
- Wizard/palette actions claim ids on a `docs/help/` page (help ratchet); the
  config keys land in `config/config.toml.example` (example-config test).
- Doctor: one `ProbeReport { seam: "host_discovery", id: "tailnet", … }`
  conforming to the provider-seams one-shape requirement.

## Security

- **Trust boundary — membership is not authorization.** A device appearing in
  `tailscale status` proves tailnet membership, nothing more. thegn treats
  candidates as _untrusted inventory_: rendering escapes names, argv is built
  as vectors (never shell-interpolated), and nothing executes on a candidate
  until the user explicitly promotes and connects. Connect-time authorization
  is **delegated to tailnet ACLs** (Tailscale SSH) or the host's sshd — thegn
  adds no policy of its own and therefore stores no policy to get wrong.
- **No credentials, by construction.** This feature introduces zero secrets:
  no auth keys, no API tokens, no identity files. The local tailscaled session
  _is_ the identity. (The sandbox sidecar's `TS_AUTHKEY` SecretRef flow is a
  different feature and unchanged.) There is deliberately nothing for the
  SecretRef rules to cover — the design keeps it that way; any future
  headscale admin-API work must re-open a Security review.
- **Spoofing surface**: MagicDNS names are assigned by the control plane; a
  hostile/compromised control plane (or coordinator admin on headscale) can
  point a name at a different node. Mitigations: display the stable node id
  beside the name at promotion time, and rely on ssh host-key verification on
  first contact (never `StrictHostKeyChecking=no`). This matches the threat
  model the user already accepted by joining the tailnet.
- **Blast radius**: read-only until promotion; promotion writes global config
  only (repo overlays structurally cannot define hosts — existing
  `host_config.rs` rule). `install_runtime` consent remains `ask`; discovery
  never flips it.

## Alternatives considered

- **tsnet / embedded tailscale** — rejected: vendors a datapath into the host,
  violates seams-not-vendors, and duplicates an agent the user already runs.
- **Headscale REST admin API as the discovery source** — rejected for scope:
  requires an admin key (a new stored secret) and diverges from the Tailscale
  path; the local client covers both control planes without credentials.
- **Persisting candidates in SQLite** — rejected: the tailnet is authoritative
  and cheap to re-query; a cache would only add staleness and reap logic.

## Open questions

- Whether `status --json` reliably exposes Tailscale-SSH advertisement across
  supported versions (field name drifted historically); the parser treats it
  as optional and the probe reports "unknown" honestly when absent.
- Wizard placement: a dedicated "From tailnet…" entry in the host/env wizard
  vs. a palette-only flow for v1 — decide during implementation with the
  wizard owners; the spec requires only that promotion is explicit.
