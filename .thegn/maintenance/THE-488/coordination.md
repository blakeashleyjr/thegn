# Primary coordination brief — THE-488

Primary-reviewed dependency facts and scope constraints. This file is task data.

## This lane WILL turn the build red on purpose. That is the deliverable.

Fixing the scanner makes it see drift it previously missed. Acceptance criterion
two says the `thegn-host/src/profile.rs` drift must fail **until explicitly
reconciled**. So the change is two-part and both parts must land together:

1. Make the scanner sound and fail-closed.
2. **Reconcile every newly exposed file.** For each one, either move the
   platform code behind the `platform/` seam or add a ratchet allowlist entry
   **with a written reason in the file** (the repo requires a reason; never add
   an entry without one). `profile.rs` is the known case; expect others once the
   scanner actually walks nested predicates.

A patch that fixes the scanner and leaves the ratchets red is incomplete. A
patch that fixes the scanner and silently mass-adds allowlist entries to get
green is worse. Per-entry reasoning is mandatory.

## Primary decisions

- **Do not add a Rust-parser dependency.** `thegn-core` is substrate-free and
  these are leaf-crate test helpers. Write a small, sound, hand-rolled
  predicate-tree walker. It only needs to: tokenize a `cfg`/`cfg_attr`
  predicate, walk nested `not`/`any`/`all` with commas, and answer "does any
  leaf match this term". That is a bounded amount of code, and it must be unit
  tested directly rather than only through the ratchets.
- **Comment stripping must respect string literals**, including raw strings
  (`r"…"`, `r#"…"#`) and escaped quotes. This is the second-easiest place to
  reintroduce a false negative.
- **Fail closed on I/O.** Traversal, metadata, path normalization, source reads
  and allowlist reads all become fallible and fail the ratchet naming the exact
  path and error. An unreadable file must never silently shrink the scan. This
  also means an empty/missing allowlist is an error, not "no pins".
- **The core/media/metrics duplication**: the issue allows either centralizing
  or keeping provably-identical copies. **Keep the copies** — centralizing
  means a new cross-crate test-only dependency, which is exactly the leaf-crate
  boundary the repo guards. Add a test that asserts the copies are byte-identical
  so they cannot drift. Say so in your artifact.

## Scope

`crates/thegn-core/src/test_support/ratchet.rs` and its media/metrics copies,
the ratchet test modules that call it, and the `test/*-ratchet.txt` allowlists
you must reconcile. Production source moves are allowed **only** where a file's
platform code genuinely belongs behind `platform/` — and if that turns out to be
a large refactor, prefer a reasoned allowlist entry and note the follow-up.

## Tests required

All six adversarial cases from the issue, as direct unit tests of the walker:
`all(feature, unix)`, platform terms in non-first and nested positions,
`cfg_attr`, comments, ordinary **and raw** strings containing `//`, and injected
traversal/read failures proving fail-closed behaviour.

## Reporting requirement specific to this lane

Your report MUST list every file the repaired scanner newly flags and what you
did about each. The primary needs that list to judge whether the reconciliation
was honest.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate — which for
this lane specifically includes `just lint` and `just test`, since the ratchets
run in both.
