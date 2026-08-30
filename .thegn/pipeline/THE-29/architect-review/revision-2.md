# THE-29 architect revision 2

Status: REVISE

The following findings still block approval. Each fix should include a focused
regression test in the same change.

## 1. The retained configured-agent recipe can record a stale provider

- `crates/thegn-host/src/daemon/service.rs:431-440` derives the memory-resident
  `DaemonRecipe::Agent` from `self.config` before the request's fresh config is
  loaded.
- The actual launch at `crates/thegn-host/src/daemon/service.rs:447-463` uses
  `config_source::fresh()` and can therefore run a different provider/harness
  than the recipe records. A subsequent live fork trusts that retained recipe
  (`crates/thegn-host/src/daemon/fork.rs:355-360`), so a config change between
  daemon startup and `sessions.open` can make a real Codex launch look like a
  Claude source (or reject the later fork as a false harness mismatch).

Fix expected: derive the retained agent recipe from the same effective,
per-request config used by `agent_open::resolve`, or return the effective
resolved harness metadata from that resolution and retain it. Add a regression
test that changes the configured provider after daemon startup, opens the
agent, and verifies that a later fork uses the provider actually launched.

## 2. The required successful native-harness path is not covered

- `crates/thegn-host/src/daemon/agent_open.rs:538-560` tests the configured
  provider mismatch, but the final `validate_fork_harness` assertion only proves
  that matching names are accepted; it does not exercise `resolve_fork` or
  verify that the source harness command survives launch composition.
- The daemon integration at
  `crates/thegn-host/src/daemon/service.rs:2193-2349` covers a raw recipe only.
  There is no hermetic configured-agent/native-harness success test for the
  actual fork path.

Fix expected: add a focused matching-provider test through `resolve_fork` (or
the daemon control path) that asserts the source harness command is preserved
while current sandbox/credential composition is applied, and that the child
retains the expected lineage. Keep it vendor-independent and state-isolated.
