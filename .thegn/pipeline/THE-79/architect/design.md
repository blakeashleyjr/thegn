# THE-79 — Put podman events behind the sandbox seam

**Issue:** THE-79 — `crates/thegn-host/src/sandbox_events.rs` hard-codes `podman` outside the
sandbox provider seam (THE-77 finding F7).
**Branch:** `tg/the-79-podman-seam` · **Role:** Architect · **Status:** design complete, 3 chunks.

---

## 1. Problem, with evidence

The sandbox audit subscriber (live `exec`/`die`/`network` events for the audit panel) is a
**vendor transport living in the host crate**:

| Evidence                                          | Site                                                                                              |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| PATH probe names the vendor binary                | `crates/thegn-host/src/sandbox_events.rs:38` — `thegn_core::util::have("podman")`                 |
| The `podman events` argv is assembled in the host | `sandbox_events.rs:77` and `:148` — `Command::new("podman")` ×2                                   |
| The only wire-up is one startup call              | `crates/thegn-host/src/run.rs:1002-1004` — `sandbox_events::spawn(cfg.sandbox.network_audit, tx)` |
| The drain side is already vendor-free             | `run.rs:9816-9822` — drains `SandboxEventBatch`, ticks `WakeSource::Container`, marks dirty       |

Every other substitutable backend is a _seam_ (`docs/ARCHITECTURE.md` §5, `openspec/specs/provider-seams`):
object-safe trait, caps ⇔ optional ops, kind implemented-or-reserved, `Probe` in `thegn doctor`,
vendor CLIs invoked **only inside their implementation files** — pinned by the `forge-leak`
ratchet (`test/forge-leak-ratchet.txt`, enforced at `justfile:571`, regenerated at
`justfile:251`). The sandbox seam's "trait" row is the `thegn_core::sandbox::Backend` enum, whose
per-backend decisions all derive from **one profile table** (`crates/thegn-core/src/sandbox.rs:412`
`BackendProfile`; arms at `sandbox.rs:237-334`; `openspec/specs/sandbox`: "Backends are described by
one profile table … every per-backend decision MUST derive from it"). Container events is a
per-backend capability — it belongs in that table and behind a seam op, not as a host-side
hard-coded binary.

The seam vocabulary to honor (`crates/thegn-core/src/seam.rs:13-19`):

> An optional operation exists iff it has a caps bit … a config `kind` value is either implemented
> or `reserved` … every implementation can describe itself (`Probe`).

## 2. Design

### 2.1 The caps bit: `EventsCap` as a `BackendProfile` column

`BackendProfile` (the sandbox seam's caps struct) gains one field, defined in the new core seam
module so sandbox.rs only grows the table column (god-file guidance — no new logic in `sandbox.rs`):

```rust
// crates/thegn-core/src/sandbox_events.rs
/// The container-events op of the sandbox seam. A kind is either implemented
/// or reserved with a reason — the seam rule (seam.rs:13-19), per backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventsCap {
    /// Implemented: `Backend::events()` hands out a transport.
    Yes,
    /// A container runtime with a daemon event stream thegn cannot read yet.
    Reserved(&'static str),
    /// No container-runtime event stream exists (process wrappers, host shell).
    No,
}
```

Per-backend values (the implemented-or-reserved decision, one per profile arm):

| Backend                                                       | EventsCap                                                                                    |
| ------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `Podman`, `PodmanRootful`                                     | `Yes`                                                                                        |
| `Docker`                                                      | `Reserved("docker has a daemon event stream but its JSON schema differs — not implemented")` |
| `Apple`                                                       | `Reserved("the apple `container` runtime has no podman-compatible events CLI")`              |
| `Smol`                                                        | `Reserved("unverified runtime — see Backend::verified")`                                     |
| `Wsl`                                                         | `Reserved("reserved kind — no runtime behind it yet")`                                       |
| `Bwrap`, `Systemd`, `WinAppContainer`, `WinJobObject`, `None` | `No` (no daemon event stream exists)                                                         |

**Scope guard:** this reserves the _events op_ per backend. `Docker` stays a fully implemented
sandbox _backend_ (`config_enum!` `SandboxBackend` gains no value, nothing becomes config-reserved,
`config validate --strict` is untouched). No new config keys (`network_audit` exists at
`config.rs:3947`), no capability-catalog rows (this is not a Control-API/CLI/MCP op), no help-page
changes (no new actions).

### 2.2 The optional op: `ContainerEvents` (sync, object-safe)

A new core seam module `thegn_core::sandbox_events` (sibling of `sandbox_build.rs` &c.; declared in
`lib.rs` at the alphabetical slot after `sandbox_dormant`). The subscriber is process-bound and its
callers are blocking threads → a **sync** seam (`provider-seams` spec: sandbox is in the sync set;
`test/async-trait-ratchet.txt` must stay empty):

```rust
pub enum EventKind { Exec, Network }

/// Receives the "rows persisted" pulse per parsed batch. The DB write happens
/// inside the transport (it owns the vendor JSON schema); the sink only fans
/// the update out.
pub trait ContainerEventSink: Send { fn on_batch(&mut self, count: usize); }

pub trait ContainerEvents: Send {
    /// Vendor transport id (`"podman"`), for logs / doctor notes.
    fn id(&self) -> &'static str;
    /// Cheap offline probe: the transport's binary on PATH (the old
    /// `have("podman")`, relocated into the impl file).
    fn available(&self) -> bool;
    /// Blocking: runs the subscription loop on the CALLER's thread until the
    /// stream ends (EOF ⇒ reap the child and return). Stream failures end the
    /// loop silently — audit is best-effort (the existing `// audit run.rs:825`
    /// contract). Never called on the event loop.
    fn subscribe(self: Box<Self>, kind: EventKind, sink: &mut dyn ContainerEventSink);
}
```

Deliberate split — **thread ownership stays in the host**: `subscribe` blocks the _calling_
thread and never spawns. The host keeps spawning the two named threads and declaring
`crate::platform::qos::set_self(Qos::Background)` as their first statement, exactly as today
(`sandbox_events.rs:55-58, 70-73`), because the thread-QoS ratchet
(`test/thread-qos-ratchet.txt`, `long_lived_threads_declare_a_qos_class`) is a host-side invariant
and `thegn_core` has no `platform::qos`. The podman _child process_ and its
`proc_registry::register(GROUP_WATCHER, …)` + EOF reap (`sandbox_events.rs:83-97`) move with the
transport — `proc_registry` is already core.

### 2.3 The podman implementation: one impl file, no other naming site

New core file `crates/thegn-core/src/sandbox_events_podman.rs` — the only file in the change that
names the vendor binary:

- `pub fn transport(backend: Backend) -> Box<dyn ContainerEvents>` builds `PodmanEvents { prefix }`
  from the existing `backend_prefix(backend)` (`sandbox.rs:2484-2490`: plain `podman`, or
  `sudo -n podman` for rootful).
- The exec/network argv (`events --format json --filter label=io.thegn=true --filter event=…`,
  verbatim from `sandbox_events.rs:76-85, 146-156`), keyed by `EventKind`.
- `parse_podman_event(line) -> Option<RawEvent>`: the vendor JSON field mapping
  (`Name`/`Status`/`Attributes.execID`/`Attributes.network`/`Time`), moved from
  `process_exec_event`/`process_network_event`.
- Blocking loop, `proc_registry` registration, EOF reap, and the existing
  `#[expect(clippy::disallowed_methods)]` markers carried over verbatim (drop any the move leaves
  unfulfilled — clippy `-D warnings` flags unfulfilled expectations).

The vendor-agnostic half of the old host module — `RawEvent` (`container`, `kind`, `detail`, `ts`),
`persist(db, &RawEvent)` (CONTAINER_PREFIX filter, insert, the exec-stream's 7-day prune — the
current asymmetry is preserved) and `worktree_from_container_name` (plain + profiled container-name
match, agent/VPN suffix strip — `sandbox_events.rs:186-215`) — moves into the covered seam module
`thegn_core::sandbox_events`, parameterized by `&Db` (one DB open per event instead of today's two;
same rows, same APIs).

### 2.4 The factory: kind implemented-or-reserved

The registration point follows the forge pattern (`forge/mod.rs` factory, pinned as IMPL) — an
inherent impl in the seam module so `sandbox.rs` gains only the table column:

```rust
// crates/thegn-core/src/sandbox_events.rs
impl crate::sandbox::Backend {
    /// The sandbox seam's optional events op. `Some` iff the profile's cap is
    /// `Yes` (podman family); reserved and No backends answer None.
    pub fn events(self) -> Option<Box<dyn ContainerEvents>> {
        match self.profile().events {
            EventsCap::Yes => Some(crate::sandbox_events_podman::transport(self)),
            EventsCap::Reserved(_) | EventsCap::No => None,
        }
    }
}
```

### 2.5 The host: a thin orchestrator with zero vendor knowledge

`crates/thegn-host/src/sandbox_events.rs` shrinks to selection + threads + sink
(`SandboxEventBatch` stays — `run.rs:5719/9820` keep their type path):

```rust
pub fn spawn(cfg: &SandboxConfig, tx: UnboundedSender<SandboxEventBatch>) {
    let Some(b) = select_backend(cfg) else { return };
    match b.profile().events {
        EventsCap::Reserved(reason) => tracing::debug!(backend = %b.label(), reason,
            "container events: reserved"),          // honest, one line, off the hot path
        _ => {}
    }
    let Some(events) = b.events() else { return };
    if !events.available() { return; }              // old have("podman") semantics
    // Thread "sandbox-events-exec": qos Background; events.subscribe(EventKind::Exec, …)
    // + optional "sandbox-events-net" thread when cfg.network_audit.
}
```

`select_backend(cfg) -> Option<Backend>` is pure and unit-tested: explicit kind →
`Backend::from_config`; `auto` → first `backend_chain` entry (`config.rs:3840+`) whose events cap is
`Yes` — mirroring how chain resolution picks a runtime. Thread names drop the vendor word
(`podman-exec-events` → `sandbox-events-exec`); the vendor id survives only in `id()` and the
proc-registry label, both inside the impl file.

`run.rs` changes by exactly one call-site line (1004): `spawn(&cfg.sandbox, sandbox_event_tx)`.

### 2.6 Behavior deltas (deliberate, small, listed for review)

1. **Rootful events start working:** the transport now uses `backend_prefix`, so
   `podman-rootful` containers' events are read via `sudo -n podman events` (today the plain
   user stream never saw rootful containers).
2. **A docker-configured host no longer subscribes podman events** when podman happens to be
   installed (`have("podman")` gated spawn regardless of config). Selection now follows config/chain.
3. **Auto** walks the chain for the first events-capable entry instead of probing one fixed binary.
   Everything else is a verbatim move: same argv, same JSON parsing, same DB rows/prune, same drain
   (`WakeSource::Container` tick, panel dirty), same silent no-op when nothing can stream.

### 2.7 Doctor visibility

`thegn doctor`'s Providers section already prints sandbox probes via
`thegn-svc/src/seam/registry.rs:356-390` (`sandbox_backend_probe`). Chunk 3 adds the caps bit to the
report (`.with_caps(&…)` / a note line): podman → `events: exec+network audit`; reserved →
`events: reserved — <reason>`; `No` → no note. Conformance shape
(`thegn_svc::conformance::assert_report_invariants`) already guards non-empty notes.

### 2.8 The ratchet: `runtime-leak` (forge-leak's twin for container runtimes)

New shrink-only grep ratchet `test/runtime-leak-ratchet.txt`, wired exactly like `forge-leak`:

- **Pattern** (`test/ratchet.sh runtime-leak '<pattern>' crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src`):
  `Command::new\("podman"\)|Command::new\("docker"\)|have\("podman"\)|have\("docker"\)|vec!\[\s*"(podman|docker)"`
  — invocation/probe/argv-literal shapes only, not prose or labels (a `"podman"` substring grep
  would pin ~30 files of comments and status strings; forge-leak greps the call shape too).
- **Seeded allowlist** (current hit-set after chunk 1 lands, verified by `git grep`):
  - `crates/thegn-core/src/sandbox_events_podman.rs` — **IMPL**: it _is_ the podman transport.
  - `crates/thegn-core/src/sandbox_tests.rs` — **IMPL-tests**: the sandbox module's own
    live-runtime tests (`:652,:661,:730,:775`).
  - `crates/thegn-host/src/agent.rs` — **LEAK** (burn-down target): VPN teardown trial prefixes
    `vec!["podman"|"docker"]` (`agent.rs:875-877`). Pinned, not fixed, by this issue — the header
    names it as the debt register entry.
- **Wiring:** one line in the `lint` recipe next to `justfile:571`, one in `ratchet-update` next to
  `justfile:251`. Header documents the IMPL/LEAK split and the burn-down rule, in the voice of
  `test/forge-leak-ratchet.txt`.

### 2.9 Coverage

The transport is a subprocess seam: `sandbox_events_podman.rs` joins `cov_ignore`
(`justfile:516`) alongside `sandbox_preflight`/`sandbox_prefetch`/`github` — exercised by its own
unit tests (run, just not % -counted) plus the no-podman dev-shell smoke path. The seam module
`thegn_core::sandbox_events` (trait, caps, persist, worktree mapping) **stays coverage-gated** — it
is where the tests concentrate. Parsers stay testable by construction: `parse_podman_event` is pure,
`persist` takes `&Db` (in-memory `Db::open_memory`, the `tests/sandbox_audit.rs` pattern).

## 3. Invariants honored (checklist)

- **Seam shape** (`seam.rs:13-19`, `provider-seams` spec): optional op (`subscribe`) ⇔ caps bit
  (`EventsCap`); implemented-or-reserved per kind, each reserved with a reason; `Probe` visible in
  doctor; sync, object-safe trait; no delegation enum; `async-trait` ratchet untouched.
- **0% idle:** `subscribe` is called only from dedicated host threads, first statement QoS
  `Background`; the event loop sees only the existing drain. No new wake sources.
- **Profile-table rule** (`openspec/specs/sandbox`): the per-backend decision derives from the one
  table; no `match backend` re-derivation.
- **God-file guidance:** `sandbox.rs` gains only 11 table-column values; all logic lands in new
  sibling modules; `run.rs` changes one line.
- **Ratchets:** `thread-qos` (host threads still declare class), `ignored-result` (DB writes keep
  the sanctioned best-effort shape + comments), `forge-leak` untouched, new `runtime-leak` seeded
  and green.
- **Coverage gate:** seam module covered with unit tests; impl file excluded as a subprocess seam;
  `crate_boundaries` untouched (core gains no substrate).
- **Ignored Results:** the moved `let _ = child.wait()` / `.ok()` sites keep their
  `// best-effort:` comments verbatim.

## 4. Chunks

Serial order 1 → (2 ∥ 3). Chunk 1 is the atomic move (the tree compiles and behaves at its end).
Chunk 2 depends on chunk 1's final hit-set for its seed; chunk 3 depends only on chunk 1's
`EventsCap`. `justfile` is touched by chunks 1 (`cov_ignore`) and 2 (`lint`/`ratchet-update`
lines) — disjoint keys, but the same file, so they run in the declared serial order.

| Chunk | Scope                                   | Files                      | Commit subject                                                                       |
| ----- | --------------------------------------- | -------------------------- | ------------------------------------------------------------------------------------ |
| 1     | Seam + podman transport + host rewiring | core ×4, host ×2, justfile | `refactor(the-79): events op on the sandbox seam — the host stops naming podman`     |
| 2     | `runtime-leak` ratchet                  | test txt, justfile         | `test(the-79): runtime-leak ratchet pins container-runtime CLIs to their impl files` |
| 3     | Doctor events cap                       | thegn-svc registry         | `feat(the-79): doctor sandbox probe reports the events cap`                          |

## 5. Non-goals

- A docker/Apple events implementation (their `Reserved` reasons name exactly what a future
  implementer owes: a JSON schema adapter behind the same trait — one new impl file + one table
  flip, per `docs/extending/provider-impl.md`).
- Per-worktree subscriptions (the global single subscriber + label filter + DB name-mapping stays;
  attribution comes from the container name, not per-worktree streams).
- The `agent.rs` VPN-teardown prefix leak (pinned as LEAK; a separate burn-down).
- No openspec delta: the sandbox spec's profile-table requirement already mandates that per-backend
  decisions derive from the table — `EventsCap` is a new column, not a new requirement; doctor notes
  are additive and shape-guarded by `conformance`.
