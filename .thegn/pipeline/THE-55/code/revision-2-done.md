# THE-55 revision 2 completion

Fixed the sole finding in architect-review verdict 2: the source compositor
guard now uses the existing per-profile singleton lock for both default and
named profiles. Default-profile lock contention no longer silently discards
the lock result; the interactive host remains advisory and continues without
the compatibility warning for the default profile. Migration probes fail
closed when the lock state cannot be safely determined.

The move command checks this guard before opening either migration database.
The deterministic host regression test holds the existing lock for both
`default` and `work` roots, verifies the move is refused, and verifies that
the database-open boundary is not entered. Core tests also cover owner
detection for both profile kinds and unknown lock state.

## Disputed

None.

## Commits

- `b30397d9` fix(the-55): make source compositor guard reliable (revision 1)
- `6de8e961` fix(the-55): make source lock test explicit (revision 1)

## Verification

- `just quick thegn-core` — passed with isolated `/tmp` runtime/temp/target paths.
- `just quick thegn-host` — passed with isolated `/tmp` runtime/temp/target paths.
- `cargo nextest run -p thegn-host session_move` — 12/12 passed.
- `cargo nextest run -p thegn-core singleton_probe` — 2/2 passed.
- `cargo clippy -p thegn-core --tests -- -D warnings` — passed.
- `cargo clippy -p thegn-host --tests -- -D warnings` — passed.
- Commit pre-hook `treefmt` — passed.

## Unverified

- Standalone `treefmt` could not initialize because `shfmt` is absent from
  `PATH`; its default cache was also read-only until the isolated-cache retry.
- Full workspace gates (`just test`, `just lint`, `just ci`, coverage, smoke,
  and e2e) were not run, per the revision dev-loop policy.
- No migration or binary was run against a live state DB.
