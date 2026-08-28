# Chunk 3 — THE-79: `thegn doctor` shows the sandbox events cap

Read `.thegn/pipeline/THE-79/architect/design.md` §2.7 first — it is binding.

## Goal

Surface the seam's new caps bit in `thegn doctor`'s Providers section, per the seam rule that every
implementation can describe itself (`seam.rs:13-19`, `provider-seams` spec: probe reports carry
"serialized caps and notes").

## Files touched (exact paths)

| Path                                    | Action                                                                                                                                    |
| --------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-svc/src/seam/registry.rs` | EDIT — `sandbox_backend_probe` (~356-370) enriches the report with the events cap; tests extended in the same file's `#[cfg(test)]` block |

No other file changes.

## Approach

1. In `sandbox_backend_probe(b: thegn_core::sandbox::Backend)`, read the cap from the profile table:
   `b.profile().events` (`EventsCap` from `thegn_core::sandbox_events`, `Serialize`-derived by
   chunk 1).
2. Enrich the report:
   - `EventsCap::Yes` → `.note(format!("events: exec+network audit ({})", events-id))` — the id can
     come from a `Backend::events()` transport's `id()` or simply the backend label; keep it one
     short line.
   - `EventsCap::Reserved(reason)` → `.note(format!("events: reserved — {reason}"))`.
   - `EventsCap::No` → no note (notes are optional; conformance only rejects EMPTY notes).
3. Also attach structured caps so `--json` consumers see the bit:
   `.with_caps(&serde_json::json!({ "events": <bool or the reason string> }))` — pick one shape,
   document it in the test: `true` for `Yes`, the reason string for `Reserved`, `false` for `No`.
   (`ProbeReport::with_caps` is `thegn_core::seam::ProbeReport`.)
4. Keep the existing availability logic untouched — this chunk only adds the caps/note layer.

## Overlap / dependency

- **Depends on chunk 1** (`EventsCap` + `Serialize` live there). Needs nothing from chunk 2.
- **File-disjoint from chunks 1 and 2** → the Lead may run this in parallel with chunk 2 once
  chunk 1 has landed.

## Tests (scoped)

```sh
just quick thegn-svc
cargo nextest run -p thegn-svc registry
cargo nextest run -p thegn-svc conformance
```

Required new tests (in `registry.rs`'s test module, alongside
`default_config_reports_every_seam_once_and_nothing_reserved`):

- sandbox report for `backend = "podman"` carries an `events:` note and `caps.events == true`;
- sandbox report for `backend = "docker"` carries `events: reserved — …` (non-empty reason, per
  conformance) and `caps.events` is the reason string;
- sandbox report for `backend = "bwrap"` carries no `events` note and `caps.events == false`;
- the conformance invariants still hold over the whole registry (`assert_report_invariants` — run
  by the existing conformance tests; no new seam name is introduced, so `KNOWN_SEAMS` is untouched).

## Done criteria

- [ ] `thegn doctor` text output shows the events note on the sandbox row when a container runtime
      is configured (manual `cargo run -p thegn-host -- doctor` eyeball is fine; CI's smoke covers
      the section's presence).
- [ ] All scoped tests green; `just quick thegn-svc` clean.
- [ ] `git status` shows only `crates/thegn-svc/src/seam/registry.rs` modified.

**Commit subject (exact):**

```
feat(the-79): doctor sandbox probe reports the events cap
```
