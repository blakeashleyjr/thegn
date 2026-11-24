# Design

This change supersedes `add-oci-runtime-tiers`' claim that egress stays thegn-enforced through the DNS
filter. `add-oci-runtime-tiers` is amended in place (deviation 3 below), and its egress statements now read
that egress reporting is unchanged by the runtime.

No render damage channel, event-loop wake path or SQLite schema is touched: every change here is pure
reporting, pure core logic, or a launch-admission decision. No `user_version` bump.

## Posture

`standard`, the default profile, is an accident guard, not a boundary against a hostile agent: it stops
writes outside the worktree and approved paths and masks thegn's control plane, but the home is readable
and the network is direct by default. `sealed` is the hardened tier, and `sealed` with a private copy of
the workspace is the boundary tier on a shared kernel. No surface calls `standard` secure or isolated. The
user's own interactive panes keep the `standard` default (owner decision 2026-11-05).

## Vocabulary

Terms this change uses follow the programme's single vocabulary (THE-791 alignment T1, T4 and T6):

- **Launch classes.** `InteractiveOwner`, `Untrusted(kind)` and `HostTrust(kind)`. `standard` stays the
  default for `InteractiveOwner`; the other two classes are defined by `add-launch-admission`.
- **Workspace mode.** `workspace.mode` is `private-copy`, `git-worktree` or `live-mount`, specified by
  `add-private-workspaces` (capability `private-workspaces`). "Private copy" below means that mode; no
  "private clone" exists.
- **Network names.** The presets are `offline`, `agent`, `development`, `custom` and `unrestricted`, plus
  `direct` for `nat` and `host` networking (labelled "direct, unfiltered", interactive panes only). Only
  `offline` has a datapath today; the endpoint presets need `Enforce` and are not reachable until the egress
  proxy exists, so this change never reports one.
- **Egress kinds.** `Enforce`, `Translate`, `PortPinned`, `Offline`, `Unmanaged`.

## Decisions

### Capabilities and `egress_for`

`EgressKind` has five values: `Enforce`, `Translate`, `PortPinned`, `Offline`, `Unmanaged`. The two new
values are added by the shared-contracts workstream; this change owns their meaning and `egress_for`'s
rules. `egress_for` takes an `EgressInputs` (backend, placement, network, optional plan) built only through
its builders.

- A provider placement is `Translate` only when the provider kind lowers lists to a policy API and reads
  them back (`EnvProviderKind::egress_api()`, today Sprites); otherwise `Unmanaged`.
- `network = "none"` is `Offline` on a verified backend with its own network namespace placed locally or
  over ssh, and `Unmanaged` elsewhere. It is never `Enforce`. Apple is excluded because it is refused.
- A lease plan reports `PortPinned` for a port pin and for a Windows proxy-only path without peer
  evidence; a credited proxy path reports `Enforce`. No manifest exists yet, so no path is credited and
  `Enforce` is unreachable.
- A VPN attachment is evaluated as `network = "nat"`: the sidecar join replaces `--network none`, so a VPN
  is `Unmanaged`, never `Offline` or `Enforce`.
- `observability` is `Instrumented` only where the backend has a real event stream (podman today);
  `can_snapshot` follows provider kinds that checkpoint (Sprites, machine0), with no local backend;
  `can_suspend_resume` follows scale-to-zero kinds; `meters_cost` is false for `custom`.

### Matrix

`NetIsolation` gains `OwnNetns` (label `netns; rules advisory`) and `Unverified` (label `unverified (no
none)`) in place of `Enforceable`. The net cell no longer reads `EgressKind`.

### Apple refusal

`RefusalClass` gains `NetworkUnexpressible` with the stable id `network-unexpressible`. Launch admission
refuses Apple with `network = "none"` before any container is created and names a remedy.

### Hostname-rule warnings

`HostnameRulesPolicy::WarnDnsOnly` becomes `WarnUnenforced`; the policy stays warn (owner decision O14,
default warn, revisited when an allowlist exists). A provider placement is silent only when its kind has an
egress API, so the claim and the warnings cannot disagree. `egress_admission` gains `SkipUnsupported` with
a note, which every caller shows once.

### `network_audit` and VPN DNS

`network_audit` is documented as the podman `network` event stream, not a DNS or connection log, and
`config validate` warns when it is enabled and no selectable backend has an event stream. `VpnDnsMode::
FilterFront` becomes reserved.

### `dns_filter`

The pure matcher moves to `thegn_core::egress::pattern` with today's semantics (bare name is apex and
subdomains, `*.x` is strict subdomains, `*` is any). The unwired UDP server, its end-to-end test, the
`sandbox-e2e-dns` recipe and its CI run line, and every stale reference are deleted. Dated records
(`docs/audits/**`, `docs/superpowers/specs/sandbox-vpn.md`) are not rewritten.

### Reserved kinds

`smol` and `appcontainer` are reserved in `config_enum!`. `Backend::is_reserved()` keeps
`available_probe` returning `Absent` as defence in depth, and doctor reports a reserved value as reserved.
`wsl` is already reserved. `Backend::Smol` is not deleted now.

### Daytona and Fly

Daytona is marked maintenance (`live_unverified`) with a doctor line and a corrected config example; its
exec handle is not repointed here. Stale Fly comments are corrected.

### ssh multiplexing

`ControlMux` is `Auto` or `Off(reason)`. It is off on Windows (`WindowsOpenSsh`) and when the concrete
socket path exceeds the 84-byte budget (`PathTooLong`). Off emits `ControlMaster=no` and
`ControlPath=none`. No platform `cfg` is added; the host OS comes from `sandbox_backend::host_os()`.

### Stable clamp

The `Remote` arm of `clamp_to_channel` also drops `PlacementMode::K8s` envs.

### Persistence channels

Sealed profiles use `sealed_cache_mounts`: `.rustup` read-only, content-addressed stores read-only over a
private directory, every other cache a private per-worktree directory under the user cache home. Standard
and open drop the read-write `.keychain` and `.claude-profiles` carve-outs and re-bind the agents'
executable configuration read-only inside the still-writable Claude and Codex configuration directories,
creating a valid empty placeholder for a missing file (owner decision O19, default yes). Symlinked entries
are reported, not pinned.

## Deviations from the plan text

1. **`EgressInputs` has no `provider` field at W1.** The provider rule reads
   `EnvProviderKind::egress_api()` from the placement. The provider-seam workstream adds the field and the
   `EgressReport` rule without changing `egress_for`'s signature.
2. **No credited proxy path and no WFP peer evidence at W1.** `proxy_path_credited` returns false until the
   enforcement manifest exists, and `WfpProxyOnly` is `PortPinned`.
3. **`add-oci-runtime-tiers` is amended in place.** A MODIFIED or REMOVED delta cannot target it: the change
   is unarchived and its requirement is not in `openspec/specs/sandbox`, so strict validation would fail.
   Its proposal, design and delta text are edited directly, and its tasks are untouched.
4. **Prior audit finding F14 is already closed** (verified at main `cbf36fe3a`; Linear THE-804).
   Podman audit filtering uses `OWNED_LABEL_FILTER = "label=thegn.managed=true"`
   (`sandbox_manage.rs`), the create path stamps `OWNED_LABEL`, and the tests
   `argv_matches_the_moved_filters` and `create_label_and_events_filter_agree`
   (`sandbox_events_podman.rs`) pin it. WS01-P1-06 records the closure; no code changes.
   The DNS-ring half disappears with the DNS filter.
5. **The stable clamp's `Providers` arm already works.** It drops provider envs and clears `[host.*]`; only
   the k8s condition is new.
6. **The acceptance search for the deleted DNS filter excludes dated records** (`docs/audits/**`,
   `docs/superpowers/specs/sandbox-vpn.md`).
7. **Persistence pins and unpinnable entries.** systemd pins come from the git read-only-pin lowering of
   the agent-presets workstream; missing directories are created empty and pinned; missing files get a
   host placeholder created with `O_CREAT|O_EXCL` and then pinned.
8. **Doctor reads reserved values through its raw-file validator** (`config_health::collect`) plus a lenient
   `from_str_any` on `config_enum!` for chain entries.

## Rejected alternatives

- **Wire `dns_filter` into OCI and bwrap so lists become "DNS-enforced".** A name filter is not an egress
  boundary: direct IPs and other resolvers bypass it. Enforcement belongs in a separate host-side proxy, and
  shipping the filter as `Enforce` repeats the defect.
- **Report `network = "none"` as `Unmanaged`.** It conflates "no controls" with "no route"; `Offline` is
  honest and distinct, and is never `Enforce`.
- **Report port pins or peerless proxy-only paths as `Enforce`.** A port pin reaches any host on that port;
  `PortPinned` is the honest label and sealed refuses it.
- **Keep `Translate` for every provider.** thegn lowers nothing for kinds other than Sprites.
- **Report Sprites `Unmanaged` until a per-worktree `EgressReport` exists.** It under-reports a control that
  works today: Sprites lowers and reads back lists and admission refuses handoff on failure.
- **Two matcher modules.** One module at the egress path, created with today's semantics and extended in
  place.
- **Delete `Backend::Smol` now.** About 25 exhaustive-match edits that the sandbox seam workstream rewrites;
  a later item removes it after the shipped smolvm backend claims the aliases.
- **Minimal smol fix (rename the binary, keep docker argv).** smolvm shares no verbs with docker; the rename
  would make the backend reachable and broken.
- **Repoint Daytona to `daytona-v0 exec`.** The provider workstream owns the handle and the verb is
  unexecuted.
- **Switch the Unix ControlPath to `%C` hashes.** It breaks the readable template that doctor and retry
  hygiene mirror and orphans live sockets on upgrade.
- **Sealed caches on the private tmpfs `$HOME` with no persistence.** Every pane would start cold, not only
  the first per worktree.
- **Bind the agent configuration directory read-only and carve state subdirectories now.** That is the
  agent-presets workstream's preset-data design; the interim per-file pins land first.

## Collisions

"SealedTunnel profile has no direct host egress" is owned by the egress workstream (WS04-P3-10); this change
only rewords it. "Backends are described by one profile table" belongs to the sandbox seam workstream and is
not touched here. The later egress, seam and provider changes MODIFY the requirements above after this
change is archived; MODIFIED blocks are rebased on the current spec at archive time.
