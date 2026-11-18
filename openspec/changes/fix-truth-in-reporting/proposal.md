# Make sandbox and remote reporting say no more than the code does

## Summary

thegn reports more security than it delivers. This change collects every behavioural correction of
workstream WS01 of the sandbox + remote programme, so that each fix is one task here and
`openspec validate` stays green while they land in wave W1. It changes no code itself.

Defects, with the symbol that carries each (symbols are cited, not line numbers, because the sandbox
files are frozen for a later wave):

- **Egress claim.** `egress_for` in `capabilities.rs` returns `Translate` for every provider,
  `Enforce` for any OCI backend or any VPN, and otherwise `Unmanaged`. The doc on `EgressKind::Enforce`
  says "the DNS filter, the VPN sidecar, and the `tgproxy` chokepoint". All three are false: nothing
  starts the DNS filter, the VPN sidecar is not an exclusive route (`TUNNEL_ONLY_REFUSAL`), and
  `tgproxy` is the model proxy.
- **VPN overrides `network = "none"`.** `oci_create_opts_in` emits `--network container:<sidecar>`
  instead of `--network none` when a sidecar is attached, so a VPN is a route out.
- **Other capability claims.** `Capabilities` reports `Instrumented` observability for any OCI
  backend although only podman has `EventsCap::Yes`; `can_snapshot` is true for podman although
  nothing runs `podman checkpoint`; `can_suspend_resume` and `meters_cost` are true for every provider.
- **Matrix label.** `NetIsolation::Enforceable` renders `netns; rules DNS-only` although the DNS
  filter server is never started.
- **Hostname rules on providers.** `hostname_rules_note_for` returns `None` for every provider
  placement and `egress_admission` skips with no message when a provider has no egress API.
- **`network_audit` and VPN DNS.** The `network_audit` example text says it logs outbound DNS queries;
  it subscribes to podman `network` events only. `VpnDnsMode::FilterFront` documents a DNS filter
  chain that does not exist.
- **Reserved kinds.** `smol` is selectable whenever `smolmachines` is on `PATH` although its backend
  is a broken docker clone; `appcontainer` is not marked reserved.
- **Apple.** `network = "none"` emits `--network none`, which Apple `container` reads as a network
  name.
- **Stable clamp.** `Config::clamp_to_channel` leaves `PlacementMode::K8s` envs in place on stable.
- **ssh multiplexing.** `ssh_base` always emits `ControlMaster=auto`, including on Windows and for
  Unix socket paths over the `sun_path` limit.
- **Persistence channels.** Sealed profiles bind shared read-write host caches
  (`auto_cache_mounts`); standard and open bind read-write `.keychain` and `.claude-profiles` and the
  whole agent configuration directory (`xdg_carveout_report_at`), which holds hooks and settings the
  host later executes.
- **Stale claims in docs and specs.** `docs/ARCHITECTURE.md` and the `provider-seams` spec claim no
  hand-written provider delegation exists; the `sandbox` spec claims the tunnel is the worktree's only
  egress and that `SealedTunnel` routes through the tunnel; the open change `add-oci-runtime-tiers`
  claims egress "stays thegn-enforced (DNS filter)".

## What changes

- Egress reporting is capped at what is enforced: no `enforce` without a credited datapath,
  `network = "none"` reports `offline`, a VPN or any list without enforcement reports `unmanaged`.
- The matrix, doctor, `config validate`, launch warnings, help, README and config example stop
  describing hostname rules as enforced.
- Reserved kinds (`smol`, `appcontainer`, and the VPN DNS mode `filter-front`) are never selected.
- Apple `network = "none"` is refused with the stable id `network-unexpressible`.
- ssh multiplexing is off on Windows and for over-long socket paths; k8s envs are clamped on stable.
- Sealed binds no shared writable cache; standard stops writing `.keychain`, other Claude profiles and
  the agents' own hooks and settings.
- The unwired DNS filter server, its tests, recipe and CI run line are deleted; its pure matcher moves
  to `thegn_core::egress::pattern` with today's semantics.
- The open change `add-oci-runtime-tiers` is amended in place so it no longer claims DNS-filter
  enforcement (see `design.md`).

## Impact

- `tasks.md` roadmap: group AC items 365 ("Container DNS proxy") and 372 ("Block/allow lists per
  container"), group AJ item 434 ("Egress consolidation + audit"), and the programme row, group BB
  item 763.
- Specs: `sandbox` (three MODIFIED, seven ADDED) and `provider-seams` (one MODIFIED).
- Other changes: amends `add-oci-runtime-tiers` in place; `mark-unverified-backends` is folded and
  archived by WS01-P5-04, which also appends a reconciling MODIFIED block to this change.
- Release channel: truth and persistence fixes are not channel-gated and apply on stable and dev.
  No config keys, CLI verbs, catalog rows, `Feature` variants or schema changes.
- Programme: this is the truth half of THE-791 gate 1 (THE-792). DNS filtering is never reported as
  egress control and hostname rules are never described as enforced egress. Owner decision O14
  (warn or refuse for hostname rules) stays open; the default recorded here is warn.

## Non-goals

- Any egress enforcement: the proxy, the bridge, network namespace plumbing (workstream WS04).
- The enforcement manifest, the `Backend` shape and the golden suite (WS02).
- Landlock, seccomp, bwrap `--clearenv`, standard-profile cache isolation (WS03).
- The final security-model page and the agent-home split (WS11).
- WSL, Job Objects and MXC (WS06); the smolvm backend (WS07); the Daytona exec handle (WS08).
