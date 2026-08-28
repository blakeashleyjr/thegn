# Chunk 2 done — Amend ARCHITECTURE.md §2 + event-loop spec (docs)

**Commit:** `f9d28177` `docs(the-78): §2 + event-loop spec state the off-loop startup heal`
(singleton code commit, exact dictated subject; pre-commit hooks passed)
**Depends on:** chunk 1 — verified landed first (`4f3d1b46`), satisfying the SERIAL constraint.

## What landed

Files touched (exactly the two owned, nothing else):

1. `docs/ARCHITECTURE.md` §2 — the false "including at startup… runs on a
   thread under a cap" sentence replaced with the true contract, dictated
   verbatim by the chunk spec: launch path runs no synchronous subprocess I/O;
   the two startup git jobs run off-loop; the heal's bounded barrier
   (`startup_heal::HealGate`, `BARRIER_TIMEOUT_MS`) awaited by the initial
   model hydration; healed-checkout `RefreshKind::Model` + waker pulse; the
   remaining sanctioned on-loop sites (post-frame interactive `git init`,
   `src/cmd/` verbs, closures already inside `spawn_blocking`/threads); the
   host `clippy.toml` `disallowed-methods` gate as the enforceable form.
   §2's **Gate:** line extended with the clippy.toml disallowed-methods entry.
2. `openspec/specs/event-loop/spec.md` — "No blocking I/O on the loop"
   requirement's SHALL extended to cover the pre-first-frame launch path
   (dictated wording); new scenario "Startup heal runs off-loop behind a
   bounded barrier" added after the kept "Expensive setup runs off-thread"
   scenario.

## Verification

- `openspec validate --all --strict`: **171 passed, 0 failed**, `spec/event-loop`
  explicitly ✓. (`openspec` was not on PATH in this non-dev shell; ran the
  pinned Nix build directly — `/nix/store/9z1q…-openspec-1.6.0/bin/openspec` —
  with the same env/wrapper the `just openspec-validate` recipe applies:
  `OPENSPEC_TELEMETRY=0 DO_NOT_TRACK=1 … validate --all --strict`.)
- Done-criteria greps: `startup_heal::HealGate` in ARCHITECTURE.md → 1 hit;
  `Startup heal runs off-loop behind a bounded barrier` in the spec → 1 hit;
  the replaced false sentence (`including at startup`) → 0 hits.
- Names in the docs cross-checked against chunk 1's landed code:
  `crates/thegn-host/src/startup_heal.rs` (`spawn`, `HealGate`,
  `BARRIER_TIMEOUT_MS = 250`, named thread `startup-heal`,
  `platform::qos::set_self(Qos::Background)`, Model-pulse fixup) and the
  hydration gate-wait (`hydrate.rs:3271/3298`); `git init` at `run.rs:15129`
  and blame `#[expect]` at `run.rs:2817` match the "remaining sanctioned
  sites" clause; `crates/thegn-host/clippy.toml` disallowed-methods exists as
  cited.
- No code changed → no `just quick`/nextest needed beyond the doc-shape gate
  above; heavy gates deliberately not run (dev-loop policy; pre-push owns
  them).

## Unverified

- **Merge-sweep QoS wording (flag for review).** The dictated sentence says
  both startup git jobs "run on named `Background`-QoS threads". True for the
  heal (named `startup-heal` thread, `Qos::Background` self-declared). The
  merge-sweep's repo-root resolve runs inside the _existing tokio
  `spawn_blocking` task_ (`merge_sweep.rs::spawn`, the resolve moved inside by
  chunk 1 — itself off-loop, per design §4.2/§4.4), and that closure does not
  self-declare `Qos::Background`; tokio blocking-pool threads are named
  `thegn-rt-N` and stay at the default QoS class. The "off-loop / no
  synchronous subprocess before the first frame" substance of the sentence is
  accurate; the "named Background-QoS" qualifier is exact for the heal,
  approximate for the sweep. Implemented verbatim as dictated (spec says
  implement EXACTLY); reviewer can either relax the sentence or add
  `set_self(Qos::Background)` inside `merge_sweep::spawn`'s closure in a
  follow-up.
- `just lint` / treefmt in a dev shell were not run standalone, but the
  pre-commit hook that fired on the commit ran treefmt and passed, so the
  markdown is formatter-clean as committed.
- The chunk's "Tests" section lists only `just openspec-validate`; the broader
  gates (`just ci`, e2e) were intentionally not run per the lead addenda.
