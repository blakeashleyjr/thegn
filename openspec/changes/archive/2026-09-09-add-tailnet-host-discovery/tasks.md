# Tasks

## 1. Pure core (thegn-core)

- [x] 1.1 `tailnet.rs`: `HostCandidate` model + `parse_status_json` over
      captured `tailscale status --json` fixtures (Tailscale and headscale
      captures; peers online/offline, tagged, with/without SSH advertisement;
      malformed JSON → typed error) — **unit tests** to the 95% line gate.
- [x] 1.2 Candidate filtering (`online_only`, `tag_filter` glob) and the
      promotion mapping `HostCandidate → SshTarget::plain(fqdn, 22, false)` —
      **unit tests**: no identity/secret fields ever populated.
- [x] 1.3 Config: `[host_discovery.tailnet]` (`enabled`, `tag_filter`,
      `online_only`, `tailscale_bin`) with `config_enum!` kind
      (`tailnet` | reserved `mdns`, `consul`); document every key in
      `config/config.toml.example` — **unit tests**: parse, defaults,
      reserved-kind rejection message.

## 2. I/O seam (thegn-svc)

- [x] 2.1 `host_discovery/mod.rs`: `HostDiscovery` trait (object-safe,
      `BoxFuture`, `SeamError` classes) + the `tailnet` impl shelling out to
      `tailscale_bin status --json` with a bounded timeout; errors classify
      `NotInstalled` / `NotConfigured` (logged out) / `Transient`.
- [x] 2.2 Probe: `ProbeReport { seam: "host_discovery", id: "tailnet" }` —
      binary, daemon reachability, login state, tailnet/control URL, peer
      count, per-peer SSH advertisement summary in `notes`.

## 3. Surfaces (thegn-host)

- [x] 3.1 `host.discover` CATALOG row (`required_scope` read) + CLI verb
      `thegn host discover [--json]` per `docs/extending/capability.md` /
      `cli-subcommand.md`; catalog-coverage test green.
- [x] 3.2 Explicit CLI promotion into a `[host.<name>]`-shaped entry shipped as
      `thegn host discover --promote <name|fqdn>`. Promotion is
      credential-free and writes the global host definition through the pure
      `to_host_config` mapping. A TUI picker is not part of this accepted cut.
- [x] 3.3 Doctor wiring: print the probe in `thegn doctor` (via the seam
      registry — `providers_report`/`providers_json` already iterate it).
- [x] 3.4 Help: document `thegn host discover` on `docs/help/cli.md` (no new
      `ACTION_SPECS` ids were added — the CLI verb needs no action-id claim; the
      help ratchet stays green). Wizard/palette action ids land with 3.2.

## 4. Verification

- [x] 4.1 Smoke: `thegn host discover` degrades cleanly with no tailscale
      binary on PATH (message names the missing binary, exit non-zero, no
      panic) — `test/smoke.sh`.
- [x] 4.2 Archive reconciliation: parser/filter/mapping tests, the missing-client
      smoke case, and scoped nextest/clippy are the retained evidence. Feature
      commit `36da0f15`, smoke fix `5ee8a418`, and merge `4f029961` are in
      current history; the old reviewer-only full-CI reminder is not claimed.
