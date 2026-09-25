# Primary review + greenlight — THE-488

Reviewing row 534's investigation (`.thegn/pipeline/THE-488/maintenance-investigate/534.md`).

**Verdict: APPROVED to implement.** The three open questions are decided below.

The most useful finding is the scope of the damage: running a sound scan
read-only, **`profile.rs` is the only newly flagged file across all five
ratchets.** That makes the reconciliation small and this lane tractable. The
helper copies are currently byte-identical, which makes the identity test cheap
to add now.

## Decision 1 — keep the three copies, add a byte-identity test

Confirmed from the coordination brief. Do not centralize: that means a new
cross-crate test-only dependency, which is exactly the leaf-crate boundary the
repo guards. Add a test asserting the core/media/metrics copies are
byte-identical so they cannot silently drift.

## Decision 2 — fault injection through a seam, not the real filesystem

Inject I/O failures through a small trait or closure seam in the scanner. Do
**not** manipulate real filesystem permissions or rely on `/proc`, an unreadable
path, or `chmod` — those are non-deterministic across CI, containers, and root.
A closure seam keeps the helper dependency-free, keeps the tests deterministic,
and lets you assert the exact path+error propagates.

Keep the seam test-only in effect: production callers pass the real reader, so
there is no runtime cost or behaviour change.

## Decision 3 — `profile.rs` gets an allowlist entry with a written reason

Do **not** move it behind `platform/`. The `#[cfg(all(feature = "profiling", unix))]`
gate is a **feature** gate whose `unix` leaf guards a signal/flamegraph
implementation; it is not a platform abstraction seam, which is what `platform/`
exists for. Moving it would put feature-gated profiler internals in the platform
module and make both worse.

Add `profile.rs` to `test/platform-cfg-host-ratchet.txt` with a reason in the
file, along the lines of: _the profiling feature's flamegraph/signal path is
unix-only; the `unix` predicate gates a feature implementation, not a platform
abstraction, so it does not belong behind `platform/`._ The ratchet is
shrink-only, so this is a pinned debt entry, honestly labelled.

## Restated constraints

- No Rust-parser dependency. Hand-rolled, sound predicate-tree walker:
  tokenize, walk nested `not`/`any`/`all` across commas, answer "does any leaf
  match this term". Unit test the walker **directly**, not only through the
  ratchets.
- Comment stripping must respect ordinary **and raw** strings (`r"…"`,
  `r#"…"#`) and escaped quotes.
- Fail closed on every I/O: traversal, metadata, path normalization, source
  reads, allowlist reads — each fails the ratchet naming the exact path and
  error. A missing/empty allowlist is an error, not "no pins".
- Handle `cfg_attr` as well as `cfg`.

## Reporting requirement

Your report must list every file the repaired scanner newly flags and what you
did about each. Per the investigation that should be exactly one (`profile.rs`);
if the count changes once the walker is sound, say so explicitly — that is the
signal the primary needs.

## Validation

Do not run cargo/nextest/clippy. The primary runs the batch gate, which for this
lane includes `just lint` **and** `just test` since the ratchets run in both.
