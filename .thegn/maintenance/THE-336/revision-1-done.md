# THE-336 revision 1 — response to primary-revision-1.md / primary-inflight-review.md

The in-flight queue edits were replaced, not patched: byte totals were
recomputed from scratch on every call, the authority registry lived under a
second lock, sequence history was a separate unbounded-by-lifecycle map, and the
host referenced an undefined `projected_diagnostics_bytes` (974c4a1d never
compiled). Registry-bounds edits in `registry.rs` were kept.

## Item-by-item

1. **Byte conservation** — new `crates/thegn-svc/src/lsp/diagnostics_queue.rs`.
   One mutex guards registry, pending queue, loss marks, health and wake.
   `bytes` is incremental and equals Σ entry cost (publication + map key clone
   - per-root order key clone + entry header) + per-ready-root cost;
     `metadata_bytes` equals registry + document loss-mark footprint (cap
     `MAX_QUEUE_METADATA_BYTES`). Authority/sequence checks run before any key is
     cloned; stale/unknown publications never touch accounting; dequeue, eviction,
     replacement-overflow and retirement subtract exactly what they remove.
     `DiagnosticsReceiver::footprint()` recomputes both; tests assert equality after
     accepted/replaced/refused/stale/evicted/cleared/retired and after 4 000 stale
     publications near cap.
2. **Lifecycle** — generations are minted by `DiagnosticsSender::register`
   (monotonic, never reused, refuses 0/u64::MAX). `retire(root, id, gen)` only
   retires that exact generation, purges its queued data and releases capacity.
   `LspInner::reconcile_roots` is now called from run.rs at the model swap when
   the worktree-root set changes, on `spawn_blocking` (client map may be held by a
   spawn); client teardown (`kill`+`wait`) runs on a Background-QoS thread.
   Negative-cache slots are released too. Host store drops retired streams via
   `take_retirements()` (the bus's registration set), never by comparing numbers.
   `LspClient::drop` retires its own generation (idempotent). Tests: 96 sequential
   open/close roots (host), 2 000 register/retire cycles (svc), close/reopen with a
   late old-generation notification over a real pipe, fake-server reopen.
3. **Registry input bounds** — root/identity checked on borrowed values before
   cloning, per-root servers (32) and roots (32) bounded in both the bus and the
   supervisor; `[[lsp.servers]]` entries/args bounded (kept from prior worker).
4. **Store admission** — `LspDiagnostics::apply` preflights on borrowed lengths
   (key, Vec header, per item struct + duplicated file path + message + "lsp:" +
   source + code) before building items; refusal leaves old data and records a
   loss mark; marks clear only when a complete publication commits (or the stream
   retires). Incomplete empty publications never clear. Marks never evict (full
   → stream mark → overflow flag). `retained_bytes` conserved (test footprint).
5. **Wake / drain** — `wake_pending` is one obligation; empty health reports do
   not wake; `rearm_if_pending` only when work is queued. `crate::lsp::drain_diagnostics`
   is the extracted host slice: loss marks and retirements first, then ≤8 pubs /
   256 KiB / 2 ms (injected clock), first item always applied, input preempts
   between items, one-publication overshoot, re-arm on budget stop, visible
   rebuild only when the active root or health changed. Tests cover each stop
   reason, re-arm vs empty, idle zero wakes with a real bridge thread, a 4-producer
   lost-wake test, and quiet-root progress.
6. **Hygiene** — `DiagnosticSink` trait and the `mpsc::Sender` impl removed; every
   constructor (`start`, `from_io`, `*_with_identity`) publishes through the bounded
   bus under a registered authority; `start_argv` (no callers) removed. Reader and
   connect arguments bundled (`ReaderContext`, `Authority`) instead of lint allows.

Also: an over-budget response now fails its request at once (`top_level_id`
linear scan) instead of waiting the 10 s timeout; symbol projection no longer
clones the parent name per child push; signature/code-action projection stop at
the aggregate before cloning.

## Open / deferred

- THE-657 (all-host ingress budgets) and THE-658 remain separate; only the LSP
  source uses the drain seam here.
- Worktree closure is observed at the periodic model swap, not instantly.
- A client started for the `current_dir()` fallback root (no active worktree
  path) is reconciled away on the next root-set change and lazily restarted.
