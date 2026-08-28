# Chunk 3 done — THE-79: `thegn doctor` shows the sandbox events cap

**Branch:** `tg/the-79-podman-seam` · **Code commit:** `2b338aa7`
`feat(the-79): doctor sandbox probe reports the events cap` (exact spec subject)
**Status:** implemented, verified, committed.

## Shipped

| Path                                    | Change                                                                                                                                                                                                                                   |
| --------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-svc/src/seam/registry.rs` | `sandbox_backend_probe` enriches every sandbox row with the container-events cap from the profile table; new test `sandbox_report_carries_the_events_cap` in the file's `#[cfg(test)]` block. **109 insertions, no other file touched.** |

Per design §2.7 (binding) + chunk-3 approach steps 1–3:

- `EventsCap::Yes` (podman, podman-rootful) → note
  `events: exec+network audit (<transport id>)` — the id comes from
  `Backend::events()`'s `id()` (falls back to the backend label, unreachable by
  construction) — plus `caps.events == true`.
- `EventsCap::Reserved(reason)` (docker, apple, smol, wsl) → note
  `events: reserved — <reason>` plus `caps.events == <reason string>`.
- `EventsCap::No` (bwrap, systemd, winappcontainer, jobobject, host) → **no
  note** plus `caps.events == false`.
- Caps shape is documented in the test (spec step 3): `true` / reason string /
  `false`, one key `events`.

## One design decision (within the spec, flagged for review)

The enrichment is attached to `base` **before** the
`sandbox_support::classify` state match, so it also rides the
`BackendState::NotInstalled` early return (`return base`). Rationale:

- The events bit is a static property of the backend's **profile-table row**
  (the seam rule that every implementation describes itself), not live runtime
  state — an uninstalled podman still _implements_ events; availability already
  says "not installed" separately.
- It makes the required tests hermetic: with enrichment only after the early
  return, `backend = "podman"` tests would pass on runtime-equipped machines
  and fail on bare CI boxes. With this shape, **every** sandbox row always
  describes its events cap, on every path, on every machine.

Availability logic itself is untouched (same `classify` → `availability` match,
same remedy note).

## Verification

- `just quick thegn-svc` — clean (clippy lib/bin, 0 warnings).
- `cargo nextest run -p thegn-svc registry` — **20/20**, including the new
  `sandbox_report_carries_the_events_cap` (podman/docker/bwrap shapes +
  an all-`Backend::ALL` loop asserting every row's note/caps against its
  profile cap — hermetic across runtime-state and OS, which also covers the
  `NotInstalled` early return that this machine's installed runtimes can't
  reach directly).
- `cargo nextest run -p thegn-svc conformance` — **7/7**
  (`assert_report_invariants` over the whole registry; no new seam, notes
  non-empty, `KNOWN_SEAMS` untouched).
- `rustfmt --check` on the file — clean.
- **Doctor eyeball (done criterion 1),** built `thegn` and drove it:
  - `backend = "podman"` → `sandbox podman-rootless ready` +
    `events: exec+network audit (podman)`;
  - `backend = "docker"` → `sandbox docker ready` +
    `events: reserved — docker has a daemon event stream but its JSON schema
differs — not implemented`;
  - default (auto → bwrap) → `sandbox bwrap ready` with no events note;
  - `doctor --json`: sandbox row serializes `"caps": {"events": true}` (podman)
    / `"caps": {"events": "<reason>"}` (docker) — the `--json` consumer story
    verified end-to-end, not just via the unit-test serializer.
- Pre-commit hook (treefmt + shellcheck + yamllint) ran on the commit and
  passed.

## Chunk-2 findings review (per Lead addenda)

Reviewed `chunk-2-done.md`'s five listed sites: all are either the sanctioned
IMPL pattern (`sandbox_compose.rs`, test fixtures in `placement.rs`,
`sandbox_cpucap.rs`, `sandbox_dormant_tests.rs`) or LEAK-debt already pinned in
the ratchet header (`vpn/mod.rs` prefixes; `agent.rs` VPN teardown). None falls
inside chunk 3's file scope (`registry.rs` only) and none is fixed by this
chunk — left for the reviewer as chunk 2 intended. `registry.rs` itself adds no
runtime-leak pattern match (the vendor id arrives via the transport's `id()`,
no vendor literal appears in the file).

## Unverified (for the review stage)

- Full-workspace gates were not run (dev-loop policy; pre-push hook territory):
  workspace-wide clippy over test/bench targets, `just test` (nextest all
  crates), coverage, smoke, e2e. Scoped equivalents above are green; the only
  changed file is thegn-svc, whose crate-scope clippy and targeted tests pass.
- The `Unsupported`/`Unreachable` availability paths' note ordering (remedy
  note after events note) is asserted only implicitly (caps/notes presence over
  all backends), not per-state — the state → availability mapping itself is
  unchanged from before this chunk.
- treefmt was not runnable as a whole in this session (shfmt missing outside
  `nix develop`, as in chunk 2); `rustfmt --check` covered the one changed
  Rust file, and the commit hook's treefmt run passed.
