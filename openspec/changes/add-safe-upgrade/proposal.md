# Safe upgrades: stale-daemon recovery and pre-migration backup

## Why

A tarball or Homebrew upgrade leaves the old daemon running (by design), which blocks the schema migration with a bare "refusing database schema migration" error. The schema is forward-only and nothing copies the database first.

## What changes

- The old-schema lease refusal becomes a typed error. The CLI and TUI explain it in plain words and offer `thegn daemon stop`.
- `thegn daemon stop` finds the daemon without migrating.
- Before a forward migration of an existing database, a verified copy is written next to it; the newest three are kept.
