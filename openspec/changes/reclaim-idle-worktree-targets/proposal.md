# Reclaim idle worktree `target/` dirs, and stop paying for dependency debuginfo

## Summary

thegn's own build artifacts are the largest consumer of disk on a machine that
uses thegn the way it is meant to be used. A full-disk audit (2026-08-26) found
**~101 GiB across 8 copies of substantially the same crate graph**, against 39.7
GiB of actual source and git history — 61% of `~/code` was regenerable build
output.

That is a direct consequence of the product's value proposition (many concurrent
agent worktrees), so the goal is not "use less disk". It is to make **an extra
worktree cheap and self-limiting** without giving up the parallelism.

Two gaps produce essentially all of it:

1. **No lifecycle rule covers abandonment.** `[disk] auto_clean_on_merge` fires
   on PR → MERGED and `clean_on_pr_closed` on → CLOSED. Neither covers the
   common case: a worktree that is simply stale — no PR opened, or the work
   drifted away. The single largest worktree on the audited machine (16 GiB) is
   exactly this. Completion is the case that _doesn't_ fill a disk, because it
   is already handled.
2. **Every `target/` carries a quarter of its bytes in dependency debuginfo
   nobody reads.** `[profile.dev]` is already tuned (`line-tables-only`,
   `split-debuginfo = "unpacked"`, `CARGO_INCREMENTAL=0`) but the tuning stops at
   the workspace boundary. Nobody steps into `tokio`/`syntect`/`tree-sitter`
   internals during normal work, and that debuginfo is duplicated into every
   worktree at once.

A third, smaller gap: `[disk] warn_threshold_gb` has stopped carrying
information. Set to 60 against an actual ~101 GiB, it is permanently tripped —
indistinguishable from no threshold — and its documentation still claims a
statusbar badge it no longer drives (the badge became a free-space alert).

## What changes

- **`[disk] idle_clean_days = 14` (new, on by default).** A worktree with
  nothing touched in it — source _or_ build output — for 14 days has its
  `target/` reclaimed. The active worktree, one with a running build, one with
  uncommitted changes, and any `target/` under 256 MiB are exempt.
- **`[disk] reclaim_on_low_disk = true` (new, on by default).** Under genuine
  disk pressure — at or below `[stats] disk_free_critical` — evict
  least-recently-touched `target/` dirs until free space is back above
  `[stats] disk_free_warn`. This reuses thresholds the user already has, so the
  pressure rule needs no absolute-GiB figure of its own.
- **`[profile.dev.package."*"] debug = 0`.** Dependency debuginfo off. thegn's
  own crates keep `line-tables-only`, so their backtraces keep file:line.
- **`just clean-aux`.** Remove the auxiliary target subtrees `just coverage` /
  `just check-cross` / `just doc` leave behind, keeping the warm
  `debug`/`release` build.
- **`warn_threshold_gb` documented for what it actually is** — a `thegn disk`
  reporting threshold — with the reason behaviour is driven off free space
  instead.

## Measured effect

Measured on the audited machine, `cargo build --workspace` into a cold target
with a warm sccache (shared box; nice'd; one build at a time):

|                       | before    | after    | delta             |
| --------------------- | --------- | -------- | ----------------- |
| `target/`             | 4.04 GiB  | 3.05 GiB | **−24.5%**        |
| `target/debug/deps`   | 3.54 GiB  | 2.89 GiB | −18%              |
| `thegn` debug binary  | 568.7 MB  | 481.2 MB | −15.4%            |
| …of which DWARF       | 128.5 MiB | 44.6 MiB | **−65%**          |
| cold build wall-clock | 410 s     | 394 s    | unchanged (noise) |

The profile change costs no build time and multiplies across every worktree at
once. The lifecycle rules reclaim whole trees: the four provably-stale ones on
the audited machine were 20.5 GiB between them.

## Trade-offs, stated

- **An unexpected cold rebuild costs an agent mid-task real wall-clock.** A cold
  `target/` with a warm sccache measured 410 s here. Every guard in the policy
  exists so that cannot happen to a worktree anyone is using: the active one, a
  running build, and (for the idle rule) anything with uncommitted work are all
  exempt, and 14 days means abandoned, not paused.
- **The pressure rule does trade wall-clock for bytes**, deliberately, and only
  when the filesystem is at its critical line — where the alternative is a
  machine that cannot build at all.
- **Dependency frames in a debug backtrace lose file:line.** Symbol names remain,
  thegn's own frames keep file:line, and release builds were already
  `strip = true` so crash reports from a release binary are unaffected.

## Deliberately rejected

- **Making `shared_target_dir` the default.** Cargo takes an _exclusive_ flock on
  `target/<profile>/.cargo-lock` for the whole compile — measured: a second
  `cargo build` against a busy shared target printed "Blocking waiting for file
  lock on build directory" immediately and was still blocked when killed. Every
  dev-profile `build`/`test`/`clippy` across every worktree would serialize. That
  is precisely antagonistic to the parallel-worktree workflow this tool exists
  for, and the constraint is explicit: do not trade the workflow for bytes.
  It stays opt-in.
- **Relocating the gate target dir to a machine-global path.** The gate hash
  exists twice on the audited machine (22 GiB + 5.4 GiB) because each Claude
  profile carries its own `XDG_STATE_HOME`. Sharing it would be free on
  serialization grounds — the gate already flocks — but the saving is bounded at
  one duplicate per extra state home, relocating orphans a warm 22 GiB gate and
  buys a cold rebuild on the very next land, and the per-state-home split is
  currently the only thing keeping two instances' gates from checking out
  different OIDs into one worktree (the flock lives beside the worktree, so it
  does not span state homes today). The stale duplicate is an abandoned gate,
  which is a lifecycle problem, not a location problem.
- **A `reclaim_budget_gb` knob.** Defaults matter more than options here: the
  user had `shared_target_dir`, `sccache` and `clean_on_pr_closed` available for
  months and turned none of them on. A ninth number would go the same way.
  Free-space pressure needs no number and adapts to the disk.

## Impact

- Config: `[disk] idle_clean_days`, `[disk] reclaim_on_low_disk` — both
  documented in `config/config.toml.example` with env knobs
  (`THEGN_DISK_IDLE_CLEAN_DAYS`, `THEGN_DISK_RECLAIM_ON_LOW_DISK`).
- New core module `thegn_core::disk_reclaim` (pure policy, unit-tested against
  the 95% core coverage gate). `thegn_core::disk::DiskUsage` gains
  `newest_mtime`, produced by the walk that was already stat'ing every entry.
- Host: the reclaim pass runs at the tail of the existing background disk-scan
  round (`crates/thegn-host/src/measure/disk.rs`) — off the event loop, behind
  the same background-lane permit. No new wake source, no new thread.
- No new actions, keybinds, panel sections or capability entries, so the help
  and capability ratchets are untouched.
- tasks.md: group D items 48 (stale worktree GC) and 51 (per-worktree disk
  usage) — this extends both with the abandonment and free-space rules they
  lacked.
