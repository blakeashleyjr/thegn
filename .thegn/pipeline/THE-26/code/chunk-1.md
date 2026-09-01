# THE-26 chunk 1 — diagnostic bundle and redaction

## Scope

Make the debug bundle materially safer and more complete without adding a
daemon protocol, persistence, wake source, or new policy surface.

## Files touched (exact)

- `crates/thegn-core/src/log_redact.rs`
- `crates/thegn-core/src/diagnostics.rs`
- `crates/thegn-host/src/cmd/bundle.rs`

No other files are in scope. In particular, do not touch config, openspec
schemas, capability catalogs, ratchet files, or the debugger command.

## Approach

1. Add a small text-line redaction helper at the existing `log_redact` seam.
   Reuse the canonical sensitive-key predicate and existing argv-shaped
   handling (`--token VALUE`, `--token=VALUE`, and `KEY=value`). Preserve
   non-sensitive text. Keep the limitation explicit: arbitrary bare
   positional secrets cannot be recognized, so callers must not log them.
2. Apply that helper when `CrashReport::render` serializes panic text and each
   retained ring line. This protects newly written reports at the serialization
   boundary without changing the nonblocking ring behavior.
3. When `cmd/bundle.rs` copies retained crash reports, sanitize their text
   again so older files and files written before the fix do not bypass the
   boundary. Keep the existing bounded/local bundle behavior and manifest.
4. Add a deterministic `diagnostics/ring.log` (or the repo’s equivalent
   manifest-safe path) for `diagnostics::ring_snapshot()` from the current
   bundle process. Include an explicit `(none)` representation and a manifest
   entry. The content and help/docs must call it the current-process ring; it
   is not a live snapshot of another host/daemon process.
5. Add focused unit coverage for text redaction, crash rendering, historical
   report sanitization, and non-empty/empty ring bundle entries. Do not add
   sleeps, polling, or broad integration tests.

## Overlap and dependency

Independent of chunks 2 and 3; no shared files or ordering dependency. The
Lead may run this chunk in parallel with both other chunks.

## Tests to run

From the worktree, in this order:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core diagnostics`
- `cargo nextest run -p thegn-core log_redact`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host bundle`

Do not run `just test`, `just ci`, a full-workspace compile, or e2e. If a
manual `thegn` invocation is needed, use `XDG_STATE_HOME="$(mktemp -d)"`.

## Done criteria

- A bundle contains a clearly labeled current-process WARN-ring section and
  manifest entry even when the ring is empty.
- Newly rendered crash reports and copied historical reports redact the
  supported sensitive forms; safe text remains unchanged, and tests cover the
  limitation around unstructured positional secrets.
- No raw crash report text is copied into the bundle without the final
  redaction pass.
- No new config/env/CLI/control key or capability-catalog entry exists.
- Env-overlay, completion-slot, control-schema, and all help ratchets are
  verified unchanged.
- The exact commit subject is:

  `fix(the-26): harden diagnostic bundle redaction`
