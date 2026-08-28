# THE-79 — Architect review verdict

**Branch:** `tg/the-79-podman-seam` · **Reviewer:** Architect · **Date:** 2026-08-28
**Verdict:** **APPROVED** — no revision chunks. One small correction applied by the
reviewer (commit `9fb38549`, comment-only).

## Lead addenda compliance

- **`git merge main` — done first** (`7e1eb91d`). main had moved (THE-70 sidebar/doctor,
  THE-78 first-frame heal, THE-80 QoS sweep, THE-83 agents/env, lint-main); the merge was
  conflict-free. The review below is of the **full** `git diff main...HEAD`.
- **Lane docs read**, including every "Unverified" section (chunk-1 ×5, chunk-2 ×4,
  chunk-3 ×3) — each is dispositioned below.
- **Mandatory gates run on the merged tree:**
  - `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example) |
test(capability) | test(seam)'` → **37/37 passed**.
  - `cargo nextest run -p thegn-host -E 'test(complete) | test(help) |
test(catalog_tests) | test(platform_ratchet) | test(ratchet)'` → **92/92 passed**,
    including `long_lived_threads_declare_a_qos_class` (the merge brought THE-80's
    QoS sweep; the new `sandbox-events-exec`/`-net` threads coexist with it cleanly).
  - Plus, scoped to the change: thegn-core `sandbox_events*` **13/13**, thegn-host
    `sandbox_events` **4/4**, thegn-svc `registry`+`conformance` **26/26**;
    `cargo check -p thegn-host` clean; `cargo fmt --check` clean.

## Design conformance (design.md, verified against the code)

- **§2.1 caps bit** — `EventsCap` (Yes / `Reserved(&'static str)` / No) lives in
  `thegn_core::sandbox_events.rs` exactly as designed; `BackendProfile` gains the one
  `events` column; all 11 `profile()` arms carry the design's per-backend value
  (podman family `Yes`; docker/apple/smol/wsl `Reserved(reason)` with the design's exact
  wording; bwrap/systemd/win\*/host `No`). `sandbox.rs` gained table columns only — no
  logic. Scope guard held: no `SandboxBackend` value added, no config keys, no capability
  rows, no help pages (all confirmed by the diff and the green catalog/help gates).
- **§2.2/§2.3 seam + transport** — `ContainerEvents` is sync, object-safe
  (`self: Box<Self>`, `&mut dyn ContainerEventSink`), thread ownership stays in the host.
  `sandbox_events_podman.rs` is the only file naming the vendor: argv (verbatim, asserted
  by `argv_matches_the_moved_filters`), JSON mapping (`Name`/`Status`/`Attributes`/`Time`),
  PATH probe (relocated `have()` — correctly probing `prefix.last()`, i.e. `podman`, not
  `sudo`), `proc_registry` GROUP_WATCHER registration held to scope end, EOF reap.
  The old host `#[expect(clippy::disallowed_methods)]` markers were correctly **dropped**
  in the move: the `Child::wait` disallowance is host-crate-only
  (`crates/thegn-host/clippy.toml`); core has no such config, so carrying them would have
  been unfulfilled-expectation errors. The host module no longer calls any disallowed
  method.
- **§2.4 factory** — `impl Backend::events()` keys off `profile().events`; no `match
backend` re-derivation; the profile-table rule (openspec/specs/sandbox) holds.
- **§2.5 host** — thin orchestrator, zero vendor knowledge: `select_backend` (pure,
  unit-tested: explicit → `from_config`, auto → first events-capable chain entry),
  one `tracing::debug!` on `Reserved`, `available()` gate, QoS `Background` as the first
  thread statement, vendor-free thread names. `run.rs` changed by exactly the call-site
  line; the drain side is untouched.
- **§2.6 behavior deltas** — all three verified present and intended (rootful events via
  `sudo -n podman`; docker-configured hosts no longer subscribe podman streams; auto walks
  the chain).
- **§2.8 ratchet** — see "Seed deviation" below: accepted.
- **§2.9 coverage** — `sandbox_events_podman` in `cov_ignore` (justfile diff confirms
  that line only); the seam module stays gated with 8 unit tests over persist/prune
  asymmetry/name mapping/parser.
- **Invariants §3** — checked item by item: 0% idle (no new wake sources; subscribe
  callers are the dedicated threads), async-trait ratchet untouched, god-file guidance
  (sandbox.rs +20 table lines, run.rs 1 line), forge-leak clean (re-ran: 4 pinned),
  ignored-result ratchet clean (re-ran: 326 pinned; `sandbox_events.rs` remains pinned
  for its one `.ok()`), thread-qos green, crate_boundaries untouched, core gains no
  substrate (BufRead/Command/Stdio are std, already core-legal per sandbox_prefetch).

## Unverified items — disposition

1. **Chunk-1, no live subscriber run / coverage not measured / full gates deferred.**
   Accepted: equivalence is by construction and I cross-checked the moved argv, JSON
   mapping, DB shapes (`insert_container_event` args, exec-only 7-day prune —
   `exec_stream_prunes_seven_days` pins the asymmetry), and the pulse semantics
   (insert/prune failure ⇒ 0 rows ⇒ no pulse; row stays written — matches the old
   `.ok()?` shapes exactly). Coverage risk from the unreachable defensive branches
   (~4 lines of crate-wide pool) is a CI-only gate concern, low; noted as a watch item
   for the next `just ci`.
2. **Chunk-1, run.rs:997-999 stale comment** — **fixed by reviewer** (`9fb38549`),
   together with the sibling at 9818 ("Sandbox container audit events (podman
   exec/network)") — the last vendor-naming prose in the host's subscriber path.
3. **Chunk-2, shellcheck/test isolation, regen idempotency** — re-verified independently:
   recomputed the hit-set with the script's own pipeline → exactly the 7 pinned files;
   re-ran the **uncommented** negative control (`let _ = Command::new("docker")` appended
   to run.rs) → fires, exit 1; tree restored. Ratchet-update idempotency claim consistent
   (header preserved byte-for-byte, 7 pins, clean run).
4. **Chunk-3, note ordering on Unsupported/Unreachable paths** — accepted: the state →
   availability mapping is pre-existing code the chunk didn't touch; the all-backends
   loop + `assert_report_invariants` guard the additive shape.
5. **Chunk-3, doctor eyeball** — **re-verified by reviewer against the built binary**:
   `thegn doctor --json` with per-backend configs →
   docker → `caps.events` = the full reservation reason + `events: reserved — …` note;
   podman → `caps.events = true` + `events: exec+network audit (podman)` (the transport's
   `id()`, not the backend label — the fallback path is genuinely unreachable);
   bwrap/default → `caps.events = false`, no events note. All three cap shapes confirmed
   end-to-end.

## Deviations from the design — reviewed and accepted

- **Chunk-2 seed (7 entries, not the design's 3).** My §2.8 seed list under-counted: the
  pattern legitimately matches test fixtures (`placement.rs`, `sandbox_cpucap.rs`,
  `sandbox_dormant_tests.rs`) and the compose/vpn transports. The Lead's ruling (pin
  current reality, list findings, don't refactor here) is correct ratchet discipline; each
  added entry is dispositioned in the header with the right IMPL/LEAK kind
  (`sandbox_compose.rs` IMPL, three fixture files IMPL, `vpn/mod.rs` LEAK-debt alongside
  `agent.rs`). `sandbox_events_podman.rs` correctly holds **no** entry — it execs via the
  backend prefix and has no literal to pin; the header states a literal added there is a
  new violation, which is stronger than my original "IMPL pin".
- **Unlisted micro-deltas found in review (both benign, recorded here for the record):**
  1. The network-stream child is now **also registered in `proc_registry`** — the old host
     `subscribe_network` skipped registration; the unified `subscribe` registers both.
     Strictly better accounting, same label pattern.
  2. `available()` is probed once and gates both threads (old: one `have()` gate, same);
     the second `backend.events()` call per network-audit host is by design (the Box is
     consumed by `subscribe`). No semantic change.

## Verification record (reviewer-run, merged tree)

| Check                                                     | Result                                          |
| --------------------------------------------------------- | ----------------------------------------------- |
| `nextest -p thegn-core` (mandatory E-set)                 | 37/37 ✅                                        |
| `nextest -p thegn-host` (mandatory E-set)                 | 92/92 ✅                                        |
| `nextest -p thegn-core sandbox_events` (seam + transport) | 13/13 ✅                                        |
| `nextest -p thegn-host sandbox_events` (selection)        | 4/4 ✅                                          |
| `nextest -p thegn-svc registry \| conformance`            | 26/26 ✅                                        |
| `bash test/ratchet.sh forge-leak …`                       | clean (4 pinned) ✅                             |
| `bash test/ratchet.sh ignored-result …`                   | clean (326 pinned) ✅                           |
| `bash test/ratchet.sh runtime-leak …`                     | clean (7 pinned) ✅ + negative control fires ✅ |
| `cargo fmt --check` (host + core)                         | clean ✅                                        |
| `thegn doctor --json` (docker/podman/bwrap)               | caps + notes per design §2.7 ✅                 |
| Vendor leak sweep of host run.rs prose                    | clean after `9fb38549` ✅                       |

Watch items (not blockers, next `just ci` owns them): coverage % over the new seam
module's defensive branches; the two LEAK-debt burn-downs (`agent.rs` VPN-teardown
prefixes, `vpn/mod.rs` `OciRuntime` prefixes) are pinned in the runtime-leak header as
intended future work.
