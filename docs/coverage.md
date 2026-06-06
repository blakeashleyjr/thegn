# Test coverage & gates

superzej gates its **testable core** at 95% line coverage and tiers the rest of
its checks so commits stay fast while pushes stay safe.

## What's measured (and what isn't)

`just coverage` runs `cargo llvm-cov --fail-under-lines 95` over the pure-logic
core and **excludes** the I/O / command / subprocess / WASM glue, which is
exercised by `test/smoke.sh` and the e2e suite instead of unit coverage. The
exclusion is a single `--ignore-filename-regex` (see `cov_ignore` in the
`justfile`).

**Gated at 95% lines (each, and in aggregate — currently ~97%):**

- `src/config.rs` — layering, validated enums, env/flag overlays, dotted-get,
  strict validation.
- `src/keymap.rs` — chord parsing, KDL generation, managed-region splice,
  override/custom resolution, collision detection.
- `src/db.rs` — the SQLite schema/migration + every query, via an in-memory DB
  (`Db::open_memory`) plus an on-disk `open()` smoke.
- `src/diff_highlight.rs` — the syntect highlight pipeline + ANSI builder.
- `src/theme.rs` — palette helpers, agent identity, blend, kbd.
- `src/models.rs` — row types + their serialization.

**Excluded (exec / exit / daemon / subprocess seams — covered by smoke/e2e, not
unit coverage):** everything under `src/commands/`, plus `main.rs` (subprocess
dispatch), `cli.rs`, `zellij.rs`, `repo.rs`, `worktree.rs`, `sandbox.rs`,
`remote.rs`, `github.rs`, `picker.rs`, `util.rs`, `msg.rs`, `out.rs`, `log.rs`.
These either `exec()`/`exit()` (replacing or ending the process), loop forever
(daemons), or are pure orchestration of external tools (`git`/`gh`/`zellij`/
`podman`/`ssh`/`fzf`) that can't be unit-covered without those tools or a
brittle mock-exec layer. `cargo-llvm-cov` excludes only per file on stable Rust,
so a module mixing pure logic with an exec/exit seam is excluded whole (its pure
parts keep their own unit tests, they're just not in the gate). The four WASM
plugins are separate crates, excluded by construction (no `wasm32-wasip1`
instrumentation).

To widen the gate, drop a module out of `cov_ignore` and bring its unit tests up
to 95% first.

## Tiers

| Stage          | What runs                                                                      | Where                  |
| -------------- | ------------------------------------------------------------------------------ | ---------------------- |
| **pre-commit** | treefmt, clippy, `cargo test`                                                  | devenv git-hook (fast) |
| **pre-push**   | `just coverage`, `just e2e`, `just visual`                                     | devenv git-hook        |
| **CI**         | `just ci` (fmt + lint + build + plugins + test + coverage + smoke + nix-build) | authoritative          |

All e2e/visual steps sandbox `ZELLIJ_SOCKET_DIR` + `SUPERZEJ_DIR` so they never
leak into the daily session or DB.

## Visual regression

`just visual` drives the TUI through `test/visual/manifest.toml` flows in a
sandboxed zellij, captures each screen with `zellij action dump-screen`, and
diffs it against a committed golden at ≥95% cell similarity. Determinism comes
from `SZ_FAKE_STATS` / `SZ_FAKE_TIME` (frozen tabbar) and a fixed terminal size.
Capture a baseline with `just visual-update`; CI additionally renders PNG
artifacts for human review.

## Follow-ups

- A `checks.coverage` flake output (mirroring `checks.clippy`) so
  `nix flake check` enforces the gate hermetically.
- PNG artifact rendering (`vhs`) wired into CI.
