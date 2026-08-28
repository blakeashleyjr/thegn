# Chunk 1 — THE-79: container-events op on the sandbox seam (the move)

Read `.thegn/pipeline/THE-79/architect/design.md` first — it is the binding design; this file is
your work order. Do not re-litigate the shape; flag surprises to the Lead instead of improvising.

## Goal

Land the optional `events()` op on the sandbox seam (`thegn_core::seam` pattern) and move the podman
events transport out of the host, so no host file names a vendor binary. At the end of this chunk
the tree **compiles, all tests pass, and runtime behavior is unchanged** (plus the three deliberate
deltas listed in design §2.6).

## Files touched (exact paths)

| Path                                             | Action                                                                                                                                                                                                                                          |
| ------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------- | --------------------- | ------ | --------------------------------------------------------------------- |
| `crates/thegn-core/src/sandbox_events.rs`        | **NEW** — the seam: `EventsCap`, `EventKind`, `ContainerEvents`, `ContainerEventSink`, `RawEvent`, `persist`, `worktree_from_container_name`, the `impl Backend { pub fn events }` factory, unit tests                                          |
| `crates/thegn-core/src/sandbox_events_podman.rs` | **NEW** — the podman transport: `transport(backend) -> Box<dyn ContainerEvents>`, argv per `EventKind`, `parse_podman_event`, blocking subscribe loops, `proc_registry` + EOF reap, unit tests. The ONLY file in this chunk that names `podman` |
| `crates/thegn-core/src/sandbox.rs`               | EDIT — `BackendProfile` gains `pub events: EventsCap` (import from `crate::sandbox_events`); each of the 11 `profile()` arms gets its value per design §2.1 table. No other logic                                                               |
| `crates/thegn-core/src/lib.rs`                   | EDIT — `pub mod sandbox_events;` + `pub mod sandbox_events_podman;` in the alphabetical `sandbox_*` block (after `sandbox_dormant`, before `sandbox_floor`)                                                                                     |
| `crates/thegn-host/src/sandbox_events.rs`        | REWRITE (thin) — keeps `SandboxEventBatch`; adds `select_backend`, `BatchForwarder` (`ContainerEventSink` → tokio mpsc), `pub fn spawn(&SandboxConfig, tx)` spawning the two QoS-declared threads. Zero vendor names                            |
| `crates/thegn-host/src/run.rs`                   | EDIT — line 1004 only: `crate::sandbox_events::spawn(&cfg.sandbox, sandbox_event_tx);`. Do NOT touch the drain (9816-9822) or anything else                                                                                                     |
| `justfile`                                       | EDIT — line 516 `cov_ignore`: add `sandbox_events_podman` to the alternation (i.e. `…                                                                                                                                                           | sandbox_prefetch | sandbox_events_podman | remote | …`). **Touch nothing else in justfile — chunk 2 owns the other keys** |

## Approach (per module)

1. **Core seam module** — port the vendor-agnostic half of the host module verbatim:
   - `RawEvent { container, kind, detail: Option<String>, ts: i64 }`.
   - `persist(db: &Db, ev: &RawEvent) -> usize`: reject names not starting with `CONTAINER_PREFIX`;
     resolve worktree via `worktree_from_container_name`; `db.insert_container_event(...)`; prune
     `7 * 24 * 3600` **only on the exec stream** (preserve today's asymmetry — keep the moved
     comment); return rows written. Keep the sanctioned `// best-effort:` comments on `.ok()` sites.
   - `worktree_from_container_name(db: &Db, name: &str)` — moved from host `sandbox_events.rs:186-215`
     (plain + profiled name match, agent/VPN suffix strip), now taking `&Db` instead of opening its own.
   - Traits exactly as design §2.2 (`subscribe` is blocking, caller-thread, no spawning inside).
   - `impl Backend { pub fn events(self) -> Option<Box<dyn ContainerEvents>> }` per design §2.4.
2. **Podman impl** — port `subscribe_exec`/`subscribe_network` bodies into
   `ContainerEvents::subscribe` for `EventKind::Exec`/`Network`: argv from design §2.3 (same
   filters verbatim), `Command::new(&prefix[0]).args(&prefix[1..])` with the prefix from
   `crate::sandbox::backend_prefix(backend)`, piped stdout / null stderr, per-line
   `parse_podman_event` → `Db::open()` → `persist` → `sink.on_batch(1)`; `proc_registry::register(
GROUP_WATCHER, <bin> events, pid)` held to scope end; `let _ = child.wait()` on EOF. Keep the
   `#[expect(clippy::disallowed_methods)]` markers ONLY where clippy still demands them (unfulfilled
   expectations are errors under `-D warnings`).
3. **Profile column** — mechanical; `EventsCap` values per design §2.1. `EventsCap` derives
   `Debug, Clone, Copy, PartialEq, Eq, Serialize` (doctor needs `Serialize` in chunk 3).
4. **Host rewrite** — `select_backend` per design §2.5 (explicit → `Backend::from_config`; `auto` →
   first `backend_chain` entry with cap `Yes` via `Backend::parse`); `Reserved(reason)` logs one
   `tracing::debug!`; spawn threads named `sandbox-events-exec` / `sandbox-events-net`, each with
   `crate::platform::qos::set_self(Qos::Background)` as its FIRST statement (thread-qos ratchet),
   calling `events.subscribe(kind, &mut sink)`; net thread only when `cfg.network_audit`.

## Overlap / dependency

- **Run FIRST — chunks 2 and 3 build on this chunk.**
- `justfile` is touched here (`cov_ignore`) and by chunk 2 (other keys): serial, this chunk first.
- No other shared files. `run.rs`/host module are otherwise untouched by chunks 2-3.

## Tests (scoped — no full-workspace gates; the pre-push hook runs those later)

```sh
just quick thegn-core
just quick thegn-host
cargo nextest run -p thegn-core sandbox_events        # new seam + impl tests
cargo nextest run -p thegn-core --test sandbox_audit  # DB round-trip/prune still green
cargo nextest run -p thegn-host sandbox_events        # host selection tests
```

Required new tests:

- core seam module: `EventsCap` table (podman family `Yes`; docker/apple/smol/wsl `Reserved` with
  non-empty reasons; bwrap/systemd/win/none `No`); factory (`Backend::Podman` → `Some`, `id()=="podman"`,
  `Docker` → `None`); `persist` over `Db::open_memory` (exec/die/network round-trip, non-thegn name
  filtered, unresolvable container dropped, exec-path prune);
- podman impl: `parse_podman_event` happy paths (exec w/ execID, die, network) + garbage lines → `None`;
- host: `select_backend` table (auto chain → first `Yes`; explicit podman; explicit docker → `None`
  after the `Reserved` branch; `none` → `None`). **No test spawns the real subscriber** (no child
  processes in unit tests); the availability gate covers the no-podman no-op.

## Done criteria

- [ ] `git grep -nE 'Command::new\("podman"\)|have\("podman"\)' -- crates/thegn-host/src` → **empty**.
- [ ] The only `podman` invocation/probe sites in `crates/` are inside `sandbox_events_podman.rs`
      (+ the pre-existing `sandbox_tests.rs`); verified with the chunk-2 pattern.
- [ ] All scoped test commands above green; no new `test/*-ratchet.txt` entries.
- [ ] `just quick thegn-core` / `just quick thegn-host` clean (clippy `-D warnings`).
- [ ] `SandboxEventBatch` type path in `run.rs` unchanged; drain untouched.

**Commit subject (exact):**

```
refactor(the-79): events op on the sandbox seam — the host stops naming podman
```
