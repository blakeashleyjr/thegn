# Add tailnet host discovery and Tailscale SSH connect

Linear: THE-8

## Why

thegn already speaks Tailscale in two places — the per-sandbox egress VPN
sidecar (`thegn_svc::vpn`, `VpnProviderKind::Tailscale`/`Headscale`) and the
ingress share seam (`TailscaleShareConfig`, `tailscale serve` via `exec_in`) —
and its remote stack is mature: `[host.<name>]` first-class machines
(`host_config.rs`, `add-host-as-resource`), `SshTarget`/`GitLoc` control reads,
mosh interactive panes, and a `thegn serve` TCP surface whose own docs say
"reach it over a tailnet/wireguard or `ssh -L`". What is missing is the
**inbound-connect** direction: the user's tailnet already knows every machine
they own, yet thegn makes them hand-type host/port/identity into config.
Roadmap item **J 123 "Tailscale zero-config path"** is marked partial for
exactly this reason.

The deferred umbrella THE-41 (full remote audit) does not block this: the
feature rides the existing remote/provider architecture unchanged — it adds a
discovery seam in front of it and stores nothing new.

## What Changes

- **A host-discovery provider seam** (`thegn_svc::host_discovery`, seam rules
  per `docs/ARCHITECTURE.md` §5): object-safe trait, `kind` implemented-or-
  reserved, `Probe`. First kind: `tailnet` — enumerate peer devices from the
  **local** tailscale client (`tailscale status --json`; the LocalAPI socket is
  a later optional layer), yielding candidates (MagicDNS name, OS, online,
  tags, Tailscale-SSH advertised). Reserved kinds: `mdns`, `consul`.
- **Pure candidate model + parser in `thegn-core`** (`tailnet.rs`): JSON →
  `Vec<HostCandidate>` with filtering (online-only, tag glob), unit-tested
  under the 95% line gate. The subprocess execution is the I/O seam in
  thegn-svc, exercised by smoke.
- **Tailscale SSH connect path, credential-free**: promoting a candidate
  produces a `[host.<name>]`-shaped host (`reach = "ssh"`, host = the MagicDNS
  FQDN, port 22, **no identity file, no stored secret of any kind**) — when the
  target runs Tailscale SSH, authentication and authorization happen inside
  tailscaled, governed by tailnet ACLs. A target running plain sshd over the
  tailnet works through the identical `SshTarget`; the probe reports which.
- **Headscale as same-seam config**: the local client is control-plane-
  agnostic (`status --json` is identical against a headscale `login_server`),
  so headscale needs no second kind — the probe surfaces the control URL and
  any capability differences (e.g. Tailscale-SSH support) instead of assuming
  them.
- **Surfaces**: a `host.discover` capability-catalog row (CLI verb under the
  `thegn host` namespace `add-host-as-resource`/`add-cli-namespaces-and-remote-open`
  establish; `--json` list shape) and a candidate picker step in the host/env
  wizard. Promotion to a configured host is always explicit; `install_runtime`
  consent stays `ask`.
- **`thegn doctor`**: a `host_discovery`/`tailnet` probe (binary present,
  tailscaled up, logged in, tailnet name, control URL, peer count,
  Tailscale-SSH availability), conforming to the one-probe-shape requirement.
- **Config**: `[host_discovery.tailnet]` (`enabled`, `tag_filter`,
  `online_only`, `tailscale_bin`) — documented in `config/config.toml.example`
  (gate: config-key recipe + example-config test).

## Impact

- **tasks.md**: J 123 (Tailscale zero-config path — the remaining half);
  feeds J 130/131 (mobile attach / phone pairing ride the same tailnet
  reachability) without claiming them.
- **Specs**: delta on `sandbox` (where `add-host-as-resource` put the host
  lifecycle requirements) — discovery seam, credential-free connect, probe,
  catalog row. No new capability dir.
- **Crates**: `thegn-core` (+`tailnet.rs`, pure), `thegn-svc`
  (+`host_discovery/`), `thegn-host` (CLI verb, wizard step, doctor wiring).
- **DB**: **no schema change** — candidates are never persisted (the tailnet
  is the source of truth; re-query on demand). Promoted hosts land in config
  and, when `add-host-as-resource` ships, its `hosts` table — unchanged here.
- **Related in-flight changes**: `add-host-as-resource` (candidates promote
  into its `[host.<name>]` + `thegn host` namespace; this change works against
  today's `host_config.rs` either way), `add-cli-namespaces-and-remote-open`
  (verb grammar), `add-vps-providers` / `add-machine0-provider` (complementary:
  they _create_ machines, this _finds_ existing ones), `mark-unverified-backends`
  (probe-honesty precedent), `verify-sandbox-mounts` (untouched).
- **Explicit non-overlap**: the existing `[sandbox.vpn]` tailscale sidecar
  (egress) and `[share]` tailscale funnel (ingress) are unchanged; this change
  never mints or reads a `TS_AUTHKEY`.

## Non-goals

- Embedding a tailnet datapath (tsnet) — thegn shells out to the user's
  tailscale, exactly as the sandbox shells out to podman.
- Auto-connecting, auto-provisioning, or background scanning of discovered
  hosts — discovery runs only on explicit user action.
- A headscale **admin-API** client (device pre-authorization, key management)
  — that is control-plane administration, not discovery.
- Managing tailnet ACLs from thegn.
