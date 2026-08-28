# THE-80 — Chunk 1: Utility sweep (user-visible worker threads)

**Lane:** tg/the-80-qos-sweep · **Design:** `.thegn/pipeline/THE-80/architect/design.md` (§3a rows 1–5, 7, 9–12)
**Runs:** FIRST (serial before chunk 2 — both chunks edit `test/thread-qos-ratchet.txt`; all other files are disjoint).
**No behaviour change:** `platform::qos::set_self` is a no-op off macOS
(`platform/qos.rs:92-96`); on macOS it only steers core placement.

## Files touched (exact paths)

1. `crates/thegn-host/src/desktop_notify.rs`
2. `crates/thegn-host/src/handlers/plugins.rs`
3. `crates/thegn-host/src/mcp_proxy/upstream.rs`
4. `crates/thegn-host/src/notify.rs`
5. `crates/thegn-host/src/plugins.rs`
6. `crates/thegn-host/src/share.rs`
7. `test/thread-qos-ratchet.txt` — delete exactly these 6 lines:
   `desktop_notify.rs`, `handlers/plugins.rs`, `mcp_proxy/upstream.rs`,
   `notify.rs`, `plugins.rs`, `share.rs`. Do not touch any other line; chunk 2
   owns the reason rewrite.

## Approach

Add `crate::platform::qos::set_self(crate::platform::qos::Qos::…);` as the
**first statement of each thread closure**, with a one-line `//` rationale
comment above it (house style: `push_notify.rs:38-41`, `hydrate.rs:473-476`).
Use the fully-qualified path exactly as existing call sites do. Two closures
are single-expression bodies and need braces added (marked ⚠ below).

### 1. `desktop_notify.rs` — two threads

⚠ **Drain thread** (`desktop_notify.rs:27-29`, only when notifications are
disabled — parks on `recv` forever, delivers nothing → `Background`). The body
is a single-expression closure; rewrite:

```rust
        std::thread::Builder::new()
            .name("desktop-notify-drain".into())
            .spawn(move || {
                // Pure housekeeping: delivers nothing, parks on recv until the bus drops.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                while rx.recv().is_ok() {}
            })
            .ok();
```

**Delivery thread** (`desktop_notify.rs:33`, `desktop-notify` → `Utility`;
rationale per design §3a #1 — user notices the toast, never blocked):

```rust
    std::thread::Builder::new()
        .name("desktop-notify".into())
        .spawn(move || {
            // Utility: the user notices the toast land but is never blocked on it.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            while let Ok(notif) = rx.recv() {
```

### 2. `handlers/plugins.rs` — two threads, both `Utility`

`thegn-plugin-feed` (`:730`, feed bridge → visible plugin content) — insert as
first statement of the spawned closure, before `let rt = match …`:

```rust
            // Utility: forwards the control event feed to a plugin whose output the user sees.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
```

`thegn-plugin-dispatch` (`:817`, answers `host.call` request/response backing
plugin content) — insert before `let rt = match …`:

```rust
                // Utility: plugin host.call round-trips back visible plugin content.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
```

### 3. `mcp_proxy/upstream.rs` — `mcp-up-{name}` (`:89`), `Utility`

The request loop deadline-times this pump's output (`recv_timeout(remaining)`,
`upstream.rs:234`) and a deadline miss is a transport failure that feeds the
circuit breaker — demotion could turn contention into spurious failures
(design §2 refinement 2). Insert as first statement of the reader closure,
before `let reader = BufReader::new(stdout);`:

```rust
                // Utility: this pump's lag lands on deadline-enforced MCP tool calls (a
                // miss is a breaker-feeding transport failure), not on background scrapes.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
```

### 4. `notify.rs` — `notify-sound` (`:345`), `Utility`

One-shot `sh -c` paired with the toast the routing decision just emitted. Insert
as first statement of the spawned closure, before `let _ = std::process::Command::new("sh")`:

```rust
            // Utility: an audible cue paired with a toast — the user hears the result.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
```

### 5. `plugins.rs` — three threads, all `Utility`

⚠ `thegn-plugins` (`:100`, host: discovery + cadence scheduler; output is
visible plugin content). Single-expression closure; rewrite:

```rust
    let spawn = std::thread::Builder::new()
        .name("thegn-plugins".into())
        .spawn(move || {
            // Utility: resident plugin contributions render as visible UI content.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            setup_and_schedule(specs, config_dir, sessions, disabled, stop, tx, waker)
        });
```

`thegn-plugin-respawn` (`:118`) — insert before `std::thread::sleep(delay);`:

```rust
                // Utility: restores visibly-missing plugin content.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
```

`thegn-plugin-once` (`:152`) — insert before `let id = plugin.spec.manifest.id.as_str().to_string();`:

```rust
                // Utility: the user just invoked this one-shot and is waiting on its result.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
```

### 6. `share.rs` — `tgshare` (`:211`), `Utility`

⚠ Single-expression closure; rewrite (user-initiated share, watching for
`Up(url)` in the statusbar chip):

```rust
        std::thread::Builder::new()
            .name("tgshare".into())
            .spawn(move || {
                // Utility: the user started this share and is watching for its URL.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                supervise(spec, wt, port, t_tx, t_waker, t_shared)
            })
            .map_err(|e| format!("could not spawn share supervisor: {e}"))?;
```

## Tests (scoped — no full-workspace builds)

```sh
just quick thegn-host                                   # clippy lib/bin, catches type errors
cargo nextest run -p thegn-host long_lived_threads      # the ratchet: 4 entries must remain
cargo nextest run -p thegn-host desktop_notify plugins share notify
```

The `-p thegn-host` test build is crate-scoped; `long_lived_threads` is
`platform_ratchet_tests.rs::long_lived_threads_declare_a_qos_class` — it must
pass with the 6 deleted pins (stale-entry check) and the 4 survivors.

## Done criteria

- [ ] All 12 thread bodies in the 6 files declare their class as the first
      statement (grep: `grep -rn 'qos::set_self' crates/thegn-host/src/{desktop_notify.rs,handlers/plugins.rs,mcp_proxy/upstream.rs,notify.rs,plugins.rs,share.rs}` → 12 hits, 2 Background + 10 Utility).
- [ ] `test/thread-qos-ratchet.txt` contains exactly 10 non-comment lines:
      the 4 survivors (`db_task.rs`, `frame_writer.rs`, `loading/ticker.rs`,
      `pane_writer.rs`) plus `forward.rs` and `perf.rs` (chunk 2 deletes those
      two) — i.e. exactly the 6 chunk-1 lines are gone, nothing else moved.
- [ ] `just quick thegn-host` clean; the scoped nextest commands above green.
- [ ] No `#[cfg]` added anywhere (platform-cfg ratchet must stay clean — it is
      checked by `just lint`, not by the scoped tests above).
- [ ] Commit with subject **exactly**:

```
fix(the-80): declare QoS on the user-visible worker threads (utility sweep)
```
