# Chunk 1 done — THE-79: container-events op on the sandbox seam

**Commit:** `036a22d7` — `refactor(the-79): events op on the sandbox seam — the host stops naming podman`
**Branch:** `tg/the-79-podman-seam` · **Status:** complete (scoped gates green, heavy gates deferred to pre-push per dev-loop policy)

## What landed

Exactly the chunk-1 file set:

| Path                                             | Change                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/sandbox_events.rs`        | **NEW** — the seam: `EventsCap` (Yes/Reserved/No, `Serialize` for chunk 3), `EventKind`, `RawEvent`, `ContainerEventSink`, `ContainerEvents` (sync, object-safe, `subscribe(self: Box<Self>)` blocks the caller), `persist(&Db, &RawEvent) -> usize` (prefix filter, insert, 7-day prune **on the exec stream only** — asymmetry preserved), `worktree_from_container_name(&Db, …)` (plain + profiled name match, agent/VPN suffix strip — now `&Db`-parameterized), `impl Backend { pub fn events }` factory, 7 unit tests |
| `crates/thegn-core/src/sandbox_events_podman.rs` | **NEW** — the podman transport (the only file naming the vendor): `transport(backend)` over `backend_prefix` (rootful gets `sudo -n podman`), argv per `EventKind` verbatim, `parse_podman_event` (Name/Status/Attributes/Time mapping), blocking subscribe loops with `proc_registry::register(GROUP_WATCHER, "podman events", pid)` held to scope end + EOF reap, 5 unit tests                                                                                                                                            |
| `crates/thegn-core/src/sandbox.rs`               | `BackendProfile` gains `pub events: EventsCap`; all 11 `profile()` arms get their design §2.1 value (podman family `Yes`; docker/apple/smol/wsl `Reserved(reason)`; bwrap/systemd/win\*/none `No`). No other logic                                                                                                                                                                                                                                                                                                          |
| `crates/thegn-core/src/lib.rs`                   | `pub mod sandbox_events;` + `pub mod sandbox_events_podman;` after `sandbox_dormant`, before `sandbox_floor`                                                                                                                                                                                                                                                                                                                                                                                                                |
| `crates/thegn-host/src/sandbox_events.rs`        | **REWRITE (thin)** — keeps `SandboxEventBatch` (type path for run.rs unchanged); adds pure `select_backend` (explicit → `Backend::from_config`; auto → first chain entry with cap `Yes` via `Backend::parse`), `BatchForwarder` (`ContainerEventSink` → tokio mpsc), `spawn(&SandboxConfig, tx)` spawning threads `sandbox-events-exec` / `sandbox-events-net` with `platform::qos::set_self(Qos::Background)` as first statement; one `tracing::debug!` on a `Reserved` cap. Zero vendor names                             |
| `crates/thegn-host/src/run.rs`                   | Line 1004 only: `spawn(&cfg.sandbox, sandbox_event_tx)`. Drain (9816-9822) and everything else untouched                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `justfile`                                       | `cov_ignore` line only: added `sandbox_events_podman` to the alternation (subprocess seam, like `sandbox_prefetch`). Nothing else touched                                                                                                                                                                                                                                                                                                                                                                                   |

## Verification (scoped, per dev-loop policy)

- `just quick thegn-core` — clean (clippy `-D warnings`).
- `just quick thegn-host` — clean (clippy `-D warnings`).
- `cargo nextest run -p thegn-core sandbox_events` — 13/13 (7 seam + 5 impl + podman argv).
- `cargo nextest run -p thegn-core --test sandbox_audit` — 3/3 (DB round-trip/prune).
- `cargo nextest run -p thegn-host sandbox_events` — 4/4 (select_backend table).
- Cheap ratchets re-run by hand: `ignored-result` clean (325 pinned, hit-set unchanged — no new entries needed), `async-trait` clean (0), `forge-leak` clean (4); `platform_ratchet_tests` incl. thread-qos 5/5; `crate_boundaries` (core stays substrate-free) 4/4.
- rustfmt with the treefmt options (`skip_children=true --edition 2024`) on all touched `.rs`; pre-commit treefmt **Passed** on the commit.

## Done criteria

- [x] `git grep -nE 'Command::new\("podman"\)|have\("podman"\)' -- crates/thegn-host/src` → **empty** (one comment that literally contained `have("podman")` was reworded to pass the raw grep).
- [x] Across the ratchet pathspec (`thegn-host/src`, `thegn-svc/src`, `thegn-core/src`), `Command::new("podman"/"docker")` + `have(...)` survive only in `sandbox_events_podman.rs` (impl — via the prefix, no literal call shape), pre-existing `sandbox_tests.rs` (live-runtime tests) and the pinned `agent.rs:875-876` LEAK. `crates/thegn-core/tests/*` keeps its pre-existing podman/docker test helpers (outside the chunk-2 ratchet pathspec).
- [x] Scoped test commands green; **no new `test/*-ratchet.txt` entries** (the ignored-result hit-set is unchanged).
- [x] `SandboxEventBatch` type path in `run.rs` unchanged; drain untouched.

## Deliberate micro-decisions for review (all within the design's shape)

1. **EOF reap is handled, not ignored.** The moved `let _ = child.wait()` became
   `if let Err(error) = child.wait() { tracing::debug!(…) }` — the exit status still carries no
   action, but pinning `sandbox_events_podman.rs` as a NEW ignored-result entry would have violated
   the chunk's "no new ratchet entries" gate, and handling-via-tracing is the ratchet's sanctioned
   alternative. Only observable delta: a debug line if a reap ever fails (old code: silent).
2. **Prune/insert failures keep the old pulse semantics.** Old `.ok()?` shapes became
   `is_err() → 0 rows` (no panel pulse on failure, row stays written) — same observable behavior,
   no ignored Result.
3. **Two `backend.events()` calls in host `spawn`** (one per stream): `subscribe` consumes
   `Box<Self>` by design, so one Box cannot serve both threads. `available()` is still probed once,
   gating both threads (old semantics).
4. **One DB open per event** (design §2.3): the transport opens `Db::open()` per parsed line and
   hands `&Db` to `persist`/`worktree_from_container_name` — was two opens per event.

## Unverified (for the review stage)

- **No wall-clock/runtime verification of the subscriber** (no live podman exercise): behavior
  equivalence is by construction (verbatim argv/JSON/DB move) + unit tests. `just smoke`/e2e not run (not in this chunk's scope; e2e forbidden by the lead addenda).
- **Coverage % not measured** (`just coverage` is CI-only): the seam module's tests are written for
  the 95% gate, but the `insert`-failure and `prune`-failure defensive branches and
  `db.worktrees()` error path are not reachable in-memory — covered only in aggregate if at all.
- **Full gates** (`just test`, `just lint`, `just ci`) deliberately not run — pre-push owns them.
- **Chunk-2 seed note:** the design's `runtime-leak` pattern matches more files than the design's
  3-entry seed list anticipated (`placement.rs` + `sandbox_compose.rs` + `sandbox_cpucap.rs` +
  `sandbox_dormant_tests.rs` argv-literal tests, `thegn-svc/src/vpn/mod.rs`, plus
  `crates/thegn-core/tests/*` if that pathspec is widened). Also `sandbox_events_podman.rs`
  currently has **zero** hits for the call-shape pattern (its vendor naming is the `id()` literal,
  the proc-registry label, and prose) — whether it stays in the seeded allowlist depends on
  chunk 2's final pattern; `ratchet-update` regeneration would drop it. Chunk 2 owns the seed
  ("current hit-set … verified by git grep").
- **run.rs:997-999 comment** still says "Podman exec/network event subscriber … no-ops when podman
  is not installed" — the chunk spec restricted run.rs to line 1004, so it was left; it is prose
  (matches no ratchet shape) and slightly stale now.
