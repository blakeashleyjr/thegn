# Chunk 2 done — THE-79: the `runtime-leak` ratchet — **STOPPED at the spec's step-1 guard**

**Branch:** `tg/the-79-podman-seam` · **Status:** **STOPPED — Lead decision required before seeding.**
No code artifacts were created: `test/runtime-leak-ratchet.txt` does **not** exist and `justfile` is
**untouched**. The only change on the branch from this chunk is this report.

## Why STOP

`chunk-2.md` step 1 is a bright-line guard:

> Expected exactly three files after chunk 1 … **If anything else appears, STOP and report to the
> Lead — a new vendor site is a finding, not a seed.**

The guard fired. The exact verification command from the spec:

```sh
git grep -nE 'Command::new\("podman"\)|Command::new\("docker"\)|have\("podman"\)|have\("docker"\)|vec!\[\s*"(podman|docker)"' \
  -- crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src
```

The **effective ratchet hit-set** (after `ratchet.sh`'s comment-only filter, which drops `///` doc
lines) is **7 files**, not 3 — and one of the spec's expected 3 doesn't match at all:

| File                                             | Lines                                  | Kind  | Match shape                                               | Disposition **proposal** (Lead decides)                                                   |
| ------------------------------------------------ | -------------------------------------- | ----- | --------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/sandbox_compose.rs`       | 93                                     | prod  | `vec!["docker".into(), "compose".into()]` (`compose_bin`) | **IMPL** — it _is_ the compose transport ("one place to change")                          |
| `crates/thegn-svc/src/vpn/mod.rs`                | 42, 45                                 | prod  | `OciRuntime::new(vec!["podman"/"docker".into()])`         | **IMPL** — the vpn seam's own OCI transport (or LEAK if VPN should consume seam prefixes) |
| `crates/thegn-core/src/placement.rs`             | 633, 671                               | test  | `vec!["podman".to_string(), …]` argv fixtures, no exec    | IMPL-test fixture                                                                         |
| `crates/thegn-core/src/sandbox_cpucap.rs`        | 1128                                   | test  | argv fixture in `skips_oci_remote_and_double_wrap`        | IMPL-test fixture                                                                         |
| `crates/thegn-core/src/sandbox_dormant_tests.rs` | 29                                     | test  | expected `start_argv` for podman machine start            | IMPL-test fixture                                                                         |
| `crates/thegn-core/src/sandbox_tests.rs`         | 252–268, 652, 661, 730, 775, 1316–1335 | mixed | `Command::new("podman")`, `have("podman")`, argv literals | **IMPL** (as designed — live-runtime tests)                                               |
| `crates/thegn-host/src/agent.rs`                 | 875–877                                | prod  | `vec!["podman"/"docker".to_string()]` trial prefixes      | **LEAK** (as designed — the burn-down target)                                             |
| `crates/thegn-core/src/sandbox_events_podman.rs` | —                                      | prod  | **no effective match** (see Finding B)                    | cannot be seeded; header prose only                                                       |

Not in the table: `sandbox_events.rs:70` — a `///` doc comment mentioning `have("podman")`;
filtered by the ratchet, as intended.

## Finding A — the design's hit-set claim was a miscount, not a chunk-1 regression

The same grep at the **design-time commit `2dcf9fe5`** (before chunk 1) already lists every extra
file: `vpn/mod.rs` (2), `placement.rs` (2), `sandbox_dormant_tests.rs` (1), `sandbox_cpucap.rs` (1),
`sandbox_compose.rs` (1). Nothing new appeared since the design — the design's "verified by
`git grep` → exactly three after chunk 1" was wrong at authoring time. The `vec!\[…\]` alternative
was added to the pattern specifically to pin `agent.rs:875-877`, and that same alternative is what
catches the five extra files. (Chunk 1's own done-summary asserted the narrower
`Command::new`+`have`-only hit-set, which is why the vec!-shaped files never surfaced there.)

## Finding B — `sandbox_events_podman.rs` is not seedable as spec'd

Chunk 1's transport execs **via its prefix**, with no literal call shape: `Command::new(program)`
over `prefix.first()` and `have(bin)` over `prefix.last()` (`sandbox_events_podman.rs:67,79`). Its
only pattern match is a `///` doc comment at line 62, which `ratchet.sh`'s comment filter drops.
Since the script derives the allowlist from the hit-set (`RATCHET_UPDATE` mode) and reports a
pinned-but-non-matching file as a **stale entry**, the spec's expected seed containing this file
fails both ways. The file is still protected: it sits inside the ratchet pathspec, so any future
literal `Command::new("podman")` there would be a new violation — the IMPL story belongs in the
header prose, not in the entry list.

## Finding C — the spec's negative control is a no-op

The spec's test appends `// let _ = Command::new("docker");` to `run.rs` and requires the ratchet
to FAIL. But `ratchet.sh` drops comment-only lines
(`grep -vE '^[^:]+:[0-9]+:[[:space:]]*//'`), so the appended line never reaches the hit-set and the
control passes — the "must FAIL" criterion cannot be exercised as written. Corrected control
(uncommented, verified to work):

```sh
echo 'let _ = Command::new("docker");' >> crates/thegn-host/src/run.rs && \
  bash test/ratchet.sh runtime-leak 'Command::new\("podman"\)|Command::new\("docker"\)|have\("podman"\)|have\("docker"\)|vec!\[\s*"(podman|docker)"' \
    crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src; \
  git checkout -- crates/thegn-host/src/run.rs
```

## Dry-run evidence (discarded; no artifact left — `git status` clean afterwards)

With a throwaway 3-entry seed file exactly as spec'd (created and deleted in one atomic step,
never staged):

- **Seed-3 vs current tree: exits 1** — 5 `new violation` errors (`placement.rs`,
  `sandbox_compose.rs`, `sandbox_cpucap.rs`, `sandbox_dormant_tests.rs`, `vpn/mod.rs`) **plus 1
  `stale entry`** (`sandbox_events_podman.rs`). Implementing the spec literally would have shipped
  a ratchet that breaks `just lint` for everyone on the first run.
- Spec's comment-only negative control: adds **0** new violations (no-op, as predicted).
- Corrected negative control: `new violation in crates/thegn-host/src/run.rs` — the failure path
  itself works.

## Why the two implementable variants were rejected

1. **Seed the spec's 3** — proven broken above (exit 1; would red `just lint` at the next pre-push).
2. **Seed the true 7 with my proposed classification** — rejected because it unilaterally widens
   the ratchet's blessed-vendor surface. A shrink-only ratchet is a policy register: marking
   `vpn/mod.rs` and `sandbox_compose.rs` IMPL (vs. fixing them to consume seam prefixes, the way
   chunk 1 treated `sandbox_events.rs`) is exactly the adjudication the spec reserves for the Lead.

## To unblock (mechanical remainder, ~10 min once the Lead picks the seed list)

1. Lead re-issues the seed list (or blesses the proposal column above) and, if desired, the
   corrected negative control from Finding C.
2. Coder re-runs chunk-2 steps 2–4 verbatim — header per spec (IMPL/LEAK voice; note
   `sandbox_events_podman.rs` as IMPL-in-prose since it is not an entry), then:
   `RATCHET_UPDATE=1 bash test/ratchet.sh runtime-leak '<pattern>' crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src`
   and the two `justfile` lines (lint ~571 / ratchet-update ~251), byte-identical pattern+pathspecs,
   `cov_ignore` untouched.
3. Verify with the spec's commands plus the corrected negative control; commit with the spec's
   exact subject `test(the-79): runtime-leak ratchet pins container-runtime CLIs to their impl files`.

## Unverified

- No Rust or build files were changed, so no `cargo`/`just quick` gate was needed or run (per the
  chunk's own note); `shellcheck test/ratchet.sh` was skipped as moot — the script is unchanged.
- The justfile wiring (lint + ratchet-update lines), the header, and the final seeded file are
  **unimplemented**, hence unverified — blocked on the seed-list decision above.
- The dry-run conclusions were exercised against the working tree only, never through
  `just lint`/pre-push (out of scope for a coder stage).

## Hands-off / overlap compliance

Serial after chunk 1 (confirmed: `036a22d7` in history). Only
`.thegn/pipeline/THE-79/code/chunk-2-done.md` was created, staged, and committed — no other path
touched, no index contention encountered.
