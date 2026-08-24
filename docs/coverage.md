# Test coverage & gates

thegn gates its **testable core** at 95% line coverage and tiers the rest of
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

| Stage          | What runs                                                                      | Where            |
| -------------- | ------------------------------------------------------------------------------ | ---------------- |
| **inner loop** | `just quick [crate]` — clippy on lib/bin only (no tests/coverage)              | on demand (fast) |
| **pre-commit** | treefmt, shellcheck, yamllint                                                  | git-hook (fast)  |
| **pre-push**   | clippy, `cargo test`, smoke                                                    | git-hook         |
| **CI**         | `just ci` (fmt + lint + build + test + **coverage** + smoke + e2e + nix-build) | authoritative    |

Coverage (`cargo llvm-cov`) is a **CI-only** gate — it is an instrumented full
recompile (the heaviest phase) and CI re-runs it regardless, so it is no longer
on pre-push. Run `just coverage` locally on demand before opening a PR. The e2e
suite sandboxes `HOME`, the XDG dirs, git config and the daemon runtime dir
into a throwaway directory, so it never leaks into the daily session or DB.

## End-to-end (`just e2e`)

[muse](https://github.com/blakeashleyjr/muse) — Playwright for terminals —
spawns the built `thegn` in a real PTY and drives the specs under
`test/muse/specs/` (30+ files, ~60 matrix cases across profiles and sizes).
Each spec runs thegn in a pinned fixture repo (fixed commit dates ⇒ stable
hashes) under the **`THEGN_E2E=1` determinism freeze**
(`crates/thegn-host/src/e2e_freeze.rs`: stats, clock, version wordmark,
activity FSM and media badge pinned) with a fixed-prompt `sh` in the panes, so
frames are byte-stable across runs and machines. Three kinds of assertion:

- **semantic** — `expect_visible` / `expect_not_visible` / `expect_count` /
  `expect_style` on stable UI text (mode indicators, panel sections, chips);
- **snapshots** — text/styled frames diffed against the committed baselines in
  `test/muse/snapshots/` (`--ci` makes a missing baseline a failure; after an
  intentional UI change, `just e2e-update` re-records — review the diff);
- **the log guard** — a `check_file` on the case's `thegn.log` rejecting
  `ERROR` / `thread '…' panicked` / overflow / index-out-of-bounds (panics reach
  the log through the hook `thegn_core::log_trace` installs).

A failing case keeps `e2e-results/<name>__<profile>__<WxH>/` — `final.txt`,
`final.png`, `final.json` (cursor/modes), per-snapshot `*.actual` / `*.diff` /
`*.baseline` files, `result.json`, and a `trace/` directory (asciinema casts,
every stable frame, the steps with their assertions; `muse trace <dir>
--frame N` renders one). CI uploads the directory as the `e2e-results`
artifact on failure. Read it before reaching for a retry.

Two specs cover what the rest deliberately turn off: `31-daemon-panes` runs
the default daemon-backed pane route on a per-case runtime dir, and
`32-resurrect` launches twice against one state dir.

### Checking a change by hand

The same harness drives thegn interactively — the look → act → look loop an
agent uses to verify its own work:

```sh
muse session open --name tg --size 120x40 \
  --env THEGN_E2E=1 --env MUSE_READY=1 --env THEGN_NO_DAEMON=1 -- thegn
muse session wait tg --visible NORMAL
muse session send tg --key ctrl+alt+p          # chords, text, paste, mouse
muse session snap tg                           # settled screen as text
muse session snap tg --kind pixel --out s.png  # or a PNG
muse session export-spec tg --out test/muse/specs/NN-feature.yaml
muse session close tg
```

`muse mcp` serves the same verbs as MCP tools; `extensions/skills/tui-check/`
is the Claude Code skill for it. The full guide — environment, spec anatomy,
the traps, artifacts, baselines, macOS — is
[`docs/testing-with-muse.md`](testing-with-muse.md).

## Follow-ups

- A `checks.coverage` flake output (mirroring `checks.clippy`) so
  `nix flake check` enforces the gate hermetically.
- Pixel (PNG) snapshot baselines alongside the text ones once the cost of
  reviewing image diffs in PRs is settled.
