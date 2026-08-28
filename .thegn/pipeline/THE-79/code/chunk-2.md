# Chunk 2 — THE-79: the `runtime-leak` ratchet

Read `.thegn/pipeline/THE-79/architect/design.md` §2.8 first — it is binding.

## Goal

Extend the shrink-only ratchet family (forge-leak's twin) so container-runtime CLI invocation
shapes outside their implementation files fail `just lint`, seeded with the current post-chunk-1
allowlist.

## Files touched (exact paths)

| Path                            | Action                                                                                                                                                                                               |
| ------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `test/runtime-leak-ratchet.txt` | **NEW** — header + 3 seeded entries (below)                                                                                                                                                          |
| `justfile`                      | EDIT — exactly two lines: an enforcement line in the `lint` recipe's ratchet block (next to the `forge-leak` line at ~571) and the matching `RATCHET_UPDATE` line in `ratchet-update` (next to ~251) |

Do NOT touch `justfile:516` (`cov_ignore` — chunk 1 owns it) or any other recipe.

## Approach

1. **Verify the hit-set on the current tree** (chunk 1 must already be merged into the branch):

   ```sh
   git grep -nE 'Command::new\("podman"\)|Command::new\("docker"\)|have\("podman"\)|have\("docker"\)|vec!\[\s*"(podman|docker)"' \
     -- crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src
   ```

   Expected exactly three files after chunk 1: `crates/thegn-core/src/sandbox_events_podman.rs`,
   `crates/thegn-core/src/sandbox_tests.rs`, `crates/thegn-host/src/agent.rs`. If anything else
   appears, STOP and report to the Lead — a new vendor site is a finding, not a seed.

2. **Seed the allowlist** — generate, then restore/verify the header:

   ```sh
   RATCHET_UPDATE=1 bash test/ratchet.sh runtime-leak \
     'Command::new\("podman"\)|Command::new\("docker"\)|have\("podman"\)|have\("docker"\)|vec!\[\s*"(podman|docker)"' \
     crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src
   ```

   The script preserves the leading `#` block of an existing file; write the header FIRST (below),
   run the update, and check the header survived and the three entries are present.

3. **Header** — in the voice of `test/forge-leak-ratchet.txt` (IMPL vs LEAK, burn-down rule):

   ```text
   # Files that invoke a container runtime by name (`Command::new("podman")`,
   # `Command::new("docker")`, `have("podman")`, or a literal argv like
   # `vec!["podman", …]`) instead of going through the sandbox seam.
   #
   # The sandbox is a provider seam (provider-seams spec): container events,
   # management and probing talk to `thegn_core::sandbox::Backend`, and vendor
   # CLIs are invoked only inside their implementation files. Two kinds of
   # entry live here:
   #   IMPL  — files that *are* a runtime transport and legitimately exec the
   #           CLI: crates/thegn-core/src/sandbox_events_podman.rs (the events
   #           transport), crates/thegn-core/src/sandbox_tests.rs (the sandbox
   #           module's own live-runtime tests). These stay.
   #   LEAK  — everything else. Current debt:
   #           crates/thegn-host/src/agent.rs — the VPN teardown tries likely
   #           runtimes by name (`vec!["podman"|"docker"]`, agent.rs:875-877)
   #           instead of asking the seam for the backend prefixes. Burn it
   #           down in a follow-up; a NEW entry here means a host file bypassed
   #           the sandbox seam.
   #
   # SHRINK-ONLY. Enforced by `test/ratchet.sh runtime-leak` in `just lint`;
   # regenerate with `just ratchet-update` (RATCHET_UPDATE=1).
   ```

4. **justfile wiring** — the pattern and pathspecs must be byte-identical between the `lint` line
   and the `ratchet-update` line (mirror the forge-leak pair):

   ```make
   bash test/ratchet.sh runtime-leak 'Command::new\("podman"\)|Command::new\("docker"\)|have\("podman"\)|have\("docker"\)|vec!\[\s*"(podman|docker)"' crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src
   ```

   (`RATCHET_UPDATE=1` prefix on the `ratchet-update` variant — copy the forge-leak lines' shape.)

## Overlap / dependency

- **Depends on chunk 1** (the seed reflects its end state; the new impl file must exist).
- **Serial after chunk 1.** Shares `justfile` with chunk 1 (different keys) — never parallelize
  the two. File-disjoint from chunk 3 → may run in parallel with it.

## Tests (scoped)

```sh
bash test/ratchet.sh runtime-leak 'Command::new\("podman"\)|Command::new\("docker"\)|have\("podman"\)|have\("docker"\)|vec!\[\s*"(podman|docker)"' crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src   # exits 0, no output
# Negative control (must FAIL with "new violation"):
echo '// let _ = Command::new("docker");' >> crates/thegn-host/src/run.rs && \
  bash test/ratchet.sh runtime-leak 'Command::new\("podman"\)|Command::new\("docker"\)|have\("podman"\)|have\("docker"\)|vec!\[\s*"(podman|docker)"' crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src; \
  git checkout -- crates/thegn-host/src/run.rs
# Shellcheck the justfile change:
just --list   # parses; plus the pre-commit shellcheck hook covers justfile? (it does not) — run:
shellcheck test/ratchet.sh   # unchanged file, sanity only
```

No Rust changes → no `cargo` gates needed beyond `just quick` if you want a sanity compile
(nothing should change). Do NOT run `just lint` (full clippy — pre-push territory).

## Done criteria

- [ ] `test/runtime-leak-ratchet.txt` exists with the IMPL/LEAK header and exactly the 3 seeded
      files; the ratchet script exits 0 against the current tree.
- [ ] The negative control fails with a "new violation" message naming the touched file.
- [ ] `justfile` has both new lines, byte-identical pattern/pathspecs to each other and to the
      forge-leak lines' formatting; `cov_ignore` untouched.
- [ ] No other files changed (`git status` clean apart from the two paths).

**Commit subject (exact):**

```
test(the-79): runtime-leak ratchet pins container-runtime CLIs to their impl files
```
