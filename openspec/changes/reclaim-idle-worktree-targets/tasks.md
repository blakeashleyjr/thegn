# Tasks

## 1. Measure before changing anything

- [x] 1.1 Quantify cargo's build-directory lock: confirm `target/<profile>/.cargo-lock`
      is an exclusive flock held for the whole compile, by observing a second
      `cargo build` against a busy shared target dir.
- [x] 1.2 Time one cold `cargo build --workspace` in a fresh worktree with a warm
      sccache, to price "a new worktree's first build" honestly.
- [x] 1.3 Measure the `target/` composition: `debug/deps` split by artifact kind,
      DWARF section sizes in a linked binary, and the auxiliary subtrees left by
      `just coverage` / `just check-cross` / `just doc`.

## 2. The decision, made pure and testable

- [x] 2.1 New `thegn_core::disk_reclaim`: `Candidate` / `Policy` / `Pressure` /
      `Reason` / `Reclaim`, and `plan()` — idle matches first, then LRU pressure
      eviction bounded by `need_bytes()`. Deterministic ordering (ties break on
      path) — **unit tests** for every guard, both rules, the off switches, the
      floor, and the ordering.
- [x] 2.2 `idle_threshold_secs()` exposed so a caller can defer the per-candidate
      `git status` probe to the few candidates that could possibly qualify.

## 3. The measurement that feeds it

- [x] 3.1 `disk::DiskUsage` gains `newest_mtime`, accumulated in the existing
      parallel walk (which was already stat'ing every entry, so it is free).
      Recorded before the zero-length early-out, so a build's empty marker files
      still count as a touch — **unit test**.

## 4. Config

- [x] 4.1 `[disk] idle_clean_days` (default 14) and `[disk] reclaim_on_low_disk`
      (default true): `DiskConfig` fields with doc comments, `ConfigOverlay`
      fields, `apply` wiring, and `THEGN_DISK_IDLE_CLEAN_DAYS` /
      `THEGN_DISK_RECLAIM_ON_LOW_DISK` env knobs.
- [x] 4.2 Document both in `config/config.toml.example`, and correct
      `warn_threshold_gb`'s prose: it is a `thegn disk` reporting threshold, not
      the statusbar badge (which has been free-space driven for some time).
- [x] 4.3 Cover the new overlay fields in `config_overlay_apply_sets_every_field`.

## 5. Host wiring

- [x] 5.1 `measure::disk::spawn_scan` takes a `disk_reclaim::Policy` (built in
      `run.rs` from `[disk]` + `[stats]`) and runs the reclaim pass at the tail
      of the round, over only what that round measured.
- [x] 5.2 Per reclaim: `worktree::clean_target`, drop the size-cache row, write a
      `disk_cleaned` notification naming the bytes and the reason, log at info,
      and pulse the waker.

## 6. Build-output size

- [x] 6.1 `[profile.dev.package."*"] debug = 0`, with the measurements and the
      preserved-own-crate-backtraces reasoning in the comment, cross-referenced
      from the existing `[profile.dev]` note (which rejected `opt-level` on deps
      — a different trade that still stands).
- [x] 6.2 `just clean-aux`: remove the auxiliary target subtrees while keeping
      the warm `debug`/`release` build.

## 7. Docs

- [x] 7.1 CLAUDE.md: the disk-hygiene paragraph — what reclaims automatically,
      what `just clean-aux` is for, and why `shared_target_dir` is not the
      default.

## 8. Validation

- [ ] 8.1 `just quick thegn-core` / `just quick thegn-host`, the new and touched
      tests, `treefmt`, `openspec validate --strict`. The full `just ci` is the
      pre-PR gate, run once by the lander.
