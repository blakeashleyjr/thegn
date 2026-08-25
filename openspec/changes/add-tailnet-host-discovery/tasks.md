# Tasks

## 1. Pure core (thegn-core)

- [ ] 1.1 `tailnet.rs`: `HostCandidate` model + `parse_status_json` over
      captured `tailscale status --json` fixtures (Tailscale and headscale
      captures; peers online/offline, tagged, with/without SSH advertisement;
      malformed JSON → typed error) — **unit tests** to the 95% line gate.
- [ ] 1.2 Candidate filtering (`online_only`, `tag_filter` glob) and the
      promotion mapping `HostCandidate → SshTarget::plain(fqdn, 22, false)` —
      **unit tests**: no identity/secret fields ever populated.
- [ ] 1.3 Config: `[host_discovery.tailnet]` (`enabled`, `tag_filter`,
      `online_only`, `tailscale_bin`) with `config_enum!` kind
      (`tailnet` | reserved `mdns`, `consul`); document every key in
      `config/config.toml.example` — **unit tests**: parse, defaults,
      reserved-kind rejection message.

## 2. I/O seam (thegn-svc)

- [ ] 2.1 `host_discovery/mod.rs`: `HostDiscovery` trait (object-safe,
      `BoxFuture`, `SeamError` classes) + the `tailnet` impl shelling out to
      `tailscale_bin status --json` with a bounded timeout; errors classify
      `NotInstalled` / `NotConfigured` (logged out) / `Transient`.
- [ ] 2.2 Probe: `ProbeReport { seam: "host_discovery", id: "tailnet" }` —
      binary, daemon reachability, login state, tailnet/control URL, peer
      count, per-peer SSH advertisement summary in `notes`.

## 3. Surfaces (thegn-host)

- [ ] 3.1 `host.discover` CATALOG row (`required_scope` read) + CLI verb
      `thegn host discover [--json]` per `docs/extending/capability.md` /
      `cli-subcommand.md`; catalog-coverage test green.
- [ ] 3.2 Wizard/palette candidate picker → explicit promotion into a
      `[host.<name>]`-shaped entry (global config only); results delivered
      off-thread via channel + `TerminalWaker`; overlay renders as `Full`.
- [ ] 3.3 Doctor wiring: print the probe in `thegn doctor`.
- [ ] 3.4 Help: claim the new action ids on a `docs/help/` page (host page or
      a new one); help + prose ratchets green.

## 4. Verification

- [ ] 4.1 Smoke: `thegn host discover` degrades cleanly with no tailscale
      binary on PATH (message names the missing binary, exit non-zero, no
      panic).
- [ ] 4.2 Run `just ci` once (includes openspec-validate) as the pre-PR gate.
