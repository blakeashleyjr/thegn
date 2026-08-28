# Chunk 2 done — THE-79: the `runtime-leak` ratchet — **COMPLETE (per Lead decision)**

**Branch:** `tg/the-79-podman-seam` · **Status:** implemented, verified, committed.
Implements `.thegn/pipeline/THE-79/code/chunk-2.md` with the Lead's binding addenda on the
previous STOP (chunk-2-done.md as of `bf437cc9`): **a ratchet pins CURRENT reality and only
shrinks** — seed the actual effective hit-set, list the unexpected sites as findings, do not
refactor them here.

## Shipped

| Path                            | Change                                                                                                                                                      |
| ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `test/runtime-leak-ratchet.txt` | **NEW** — IMPL/LEAK header (forge-leak voice) + **7** seeded entries (the actual effective hit-set, not the spec's 3)                                       |
| `justfile`                      | Two lines: enforcement in `lint` (next to the forge-leak line) and `RATCHET_UPDATE=1` regeneration in `ratchet-update` — pattern + pathspecs byte-identical |

Nothing else touched: `cov_ignore` (justfile:516+) is not in the diff; no Rust sources changed.

## The seed: the actual effective hit-set (7 files)

Computed with ratchet.sh's own pipeline (git grep → comment-only lines dropped → sort -u):

```
crates/thegn-core/src/placement.rs
crates/thegn-core/src/sandbox_compose.rs
crates/thegn-core/src/sandbox_cpucap.rs
crates/thegn-core/src/sandbox_dormant_tests.rs
crates/thegn-core/src/sandbox_tests.rs
crates/thegn-host/src/agent.rs
crates/thegn-svc/src/vpn/mod.rs
```

Both `sandbox_events.rs:70` and `sandbox_events_podman.rs:62` match only in `///` doc comments —
filtered, as the script intends. `sandbox_events_podman.rs` therefore holds **no entry** (Finding B
from the STOP stands): it execs via its backend prefix, so there is no literal to pin, and the
header states that a literal added there is a new violation.

## Findings — the unexpected sites (for reviewer + chunk 3; NOT refactored in this chunk)

The spec expected 3 files; the seed differs from that list by the five below (the Lead's addendum
said "4"; the full set is five — all are listed so nothing is lost). One-line why-leak / why-fine:

1. **`crates/thegn-core/src/sandbox_compose.rs:93`** — `compose_bin()` returns
   `vec!["docker".into(), "compose".into()]`.
   _Why it is fine:_ it **is** the compose transport — the file is the documented "one place to
   change" for the binary tokens, the exact IMPL pattern the seam mandates. No action.
2. **`crates/thegn-svc/src/vpn/mod.rs:42,45`** — `OciRuntime::podman()/docker()` build
   `vec!["podman"|"docker".into()]` prefixes for the vpn capability's sidecar runtime.
   _Why it is a vendor leak:_ the vpn seam duplicates the backend-prefix knowledge the sandbox seam
   already owns (`oci_prefix`), so a new/renamed backend drifts these two apart; a future VPN change
   should source prefixes from the sandbox seam. Pinned as LEAK-debt in the header next to agent.rs.
3. **`crates/thegn-core/src/placement.rs:633,671`** — test fixtures (`local_is_passthrough` &c.)
   assert `Placement` argv passthrough over a `vec!["podman", …]` fixture.
   _Why it is fine:_ sandbox-module test data, no exec, no vendor call.
4. **`crates/thegn-core/src/sandbox_cpucap.rs:1128`** — test fixture
   `skips_oci_remote_and_double_wrap` builds a `vec!["podman", "exec"]` argv.
   _Why it is fine:_ asserts the cpucap wrapper's argv math over an OCI backend; no exec.
5. **`crates/thegn-core/src/sandbox_dormant_tests.rs:29`** — expected `start_argv`
   `vec!["podman", "machine", "start"]` for the dormant-sandbox wake.
   _Why it is fine:_ a test expectation pinning the wake argv; no exec.

Known companions (as designed): `crates/thegn-host/src/agent.rs:875-877` stays the **LEAK
burn-down target** (host VPN teardown tries likely runtimes by name instead of asking the seam);
`crates/thegn-core/src/sandbox_tests.rs` stays **IMPL** (the sandbox module's own live-runtime
tests).

## Verification (all against the current tree)

- **Seed correctness:** `RATCHET_UPDATE=1` rewrite → "rewrote … (7 pinned)"; header preserved
  byte-for-byte; re-running it is **idempotent** (zero diff after a second rewrite).
- **Enforcement clean:** `bash test/ratchet.sh runtime-leak '<pattern>' <3 pathspecs>` →
  `ratchet(runtime-leak): clean (7 pinned)`, exit 0.
- **Negative control — proven firing, then reverted.** The spec's control appends a
  `//`-commented line, which the script's comment filter drops (the STOP's Finding C — a no-op).
  Used the corrected uncommented control:
  ```sh
  echo '    let _ = Command::new("docker");' >> crates/thegn-host/src/run.rs
  bash test/ratchet.sh runtime-leak …   # → ERROR: new violation in crates/thegn-host/src/run.rs, exit 1
  git checkout -- crates/thegn-host/src/run.rs
  ```
  Tree restored to exactly justfile + the new txt afterwards.
- **justfile wiring:** the two new lines are byte-identical modulo the `RATCHET_UPDATE=1` prefix
  (verified by stripping the prefix and `sort -u` → 1 line); `just --list` parses; `cov_ignore`
  untouched (confirmed via `git diff justfile`).
- **Scoped Rust gate (per Lead):** `cargo nextest run -p thegn-host -E 'test(ratchet)'` →
  13/13 passed (help, caret, platform ratchets — sibling ratchet family unaffected).
- No Rust behavior changed → no `just quick`/full gates needed or run (dev-loop policy); heavy
  gates remain the pre-push hook's job.

## Spec deviations (all bound by the Lead decision)

1. Seed = the actual 7-file hit-set instead of the spec's 3 (the spec's step-1 guard fired; the
   Lead ruled current reality wins — the miscount is Finding A in the STOP report, a design-time
   issue, not a chunk-1 regression).
2. `sandbox_events_podman.rs` is absent from the entry list (spec listed it): no effective match
   exists to pin; its IMPL story is carried in the header prose instead.
3. Negative control run uncommented (spec's commented variant is provably a no-op — Finding C).
4. `shellcheck test/ratchet.sh` skipped in this session (tool not on PATH outside `nix develop`;
   the file is unchanged this chunk and is hook-covered on commit in the dev shell).

## Hands-off / overlap compliance

Serial after chunk 1 (`036a22d7` in history). Only the two spec'd paths plus this report changed;
chunk 1's `cov_ignore` and chunk 3's files untouched.
