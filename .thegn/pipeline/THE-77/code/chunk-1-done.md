# THE-77 chunk 1 — done

Re-arms the three gates ARCHITECTURE.md claims are enforced (design F1, F2, F3).
Implemented exactly as specced; no scope added, nothing deferred.

## Commits

| SHA        | Subject                                                                                                                           |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `acd06415` | `fix(the-77): ratchet.sh sorts and compares in byte order (LC_ALL=C)`                                                             |
| `cb6b9197` | `fix(the-77): re-arm three gates — ratchet.sh byte order, gix owner row, proxy workspace lints` (the exact subject from the spec) |

Split into two so the no-compile shell fix could land as soon as it was verified;
the final commit carries the verbatim subject the spec requires.

## Files changed (exactly the three the spec lists)

- `test/ratchet.sh` — +6 lines: `export LC_ALL=C` plus the comment block from the
  spec, immediately after the `set -euo pipefail` / `cd` preamble.
- `crates/thegn-core/tests/crate_boundaries.rs` — one `OWNERS` row
  (`("gix", &["thegn-svc"])`, after `alacritty_terminal`) and the new
  `every_member_inherits_workspace_lints` test.
- `crates/thegn-proxy/Cargo.toml` — `[lints] workspace = true`, placed
  immediately before `[[bin]]`, matching `thegn-host`'s placement.

No `test/*-ratchet.txt` was modified; no sibling-owned file was touched
(`git status` was clean of anything else at both commits, and only these paths
were ever `git add`ed).

## (a) F1 — ratchet.sh byte order

Took the spec's recommended form (`export LC_ALL=C` once at the top) rather than
per-pipeline `LC_ALL=C`. Not narrowed to `LC_COLLATE`.

**Reproduced first.** Before the fix, on this `en_US.UTF-8` box:

```
$ bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates
comm: file 1 is not in sorted order
comm: file 2 is not in sorted order
comm: input is not in sorted order
comm: file 1 is not in sorted order
comm: file 2 is not in sorted order
comm: input is not in sorted order
ratchet(ignored-result): clean (323 pinned)
```

(Six warning lines, not the four the spec predicted — GNU `comm` emits an extra
`input is not in sorted order` per invocation. Same defect, same fix.)

After the fix, all five bash ratchets, **zero `comm:` lines**, all `rc=0`:

```
ratchet(ignored-result): clean (323 pinned)
ratchet(forge-leak):     clean (4 pinned)
ratchet(async-trait):    clean (0 pinned)
ratchet(json-emit):      clean (14 pinned)
ratchet(element):        clean (3 pinned)
```

**Negative test** (temporarily drop the `config_resolve.rs` pin) — exactly the
outcome the spec asks for:

```
rc=1
ERROR: ratchet(ignored-result): new violation in crates/thegn-core/src/config_resolve.rs — …
       fix the file, or pin it in test/ignored-result-ratchet.txt with a reason (the list only shrinks)
```

One error line. **No** `stale entry crates/thegn-core/src/config.rs` line, which
is what the pre-fix script wrongly emitted. `git diff --stat test/` afterwards
showed only `test/ratchet.sh` — the allowlist was restored byte-identical.

`shellcheck test/ratchet.sh` clean (run inside `nix develop`; it also passed in
the pre-commit hook on `acd06415`).

## (b) F2 — gix owner row

Zero-diff as predicted: `thegn-svc` is the only member declaring
`gix.workspace = true`, so the "an owner that no longer uses the substrate should
be removed" assertion is satisfied and `substrates_are_only_used_by_their_owners`
passes unchanged. `"gix"` left in `CORE_FORBIDDEN` per the spec.

## (c) F3 — proxy workspace lints

Opt-in added, and pinned by a new sibling test to `every_member_is_covered`. It
walks `workspace.members` from the root manifest the same way `members()` does,
parses each member manifest, and asserts `manifest["lints"]["workspace"] == true`
— collecting every offender into a `Vec` and asserting once, no `panic!` on first.

## Verification run

```
cargo nextest run -p thegn-core --test crate_boundaries      →  4 tests run: 4 passed
    PASS every_member_is_covered
    PASS every_member_inherits_workspace_lints        ← new
    PASS substrates_are_only_used_by_their_owners     ← now also checks gix
    PASS core_is_substrate_free

cargo clippy -p thegn-proxy --all-targets -- -D warnings     →  Finished, no warnings
```

The proxy build is clean under the newly-armed lints — **no `let_underscore_future`
violation surfaced**, matching the design's finding that the tripwire was disarmed
rather than tripped. Nothing was `#[allow]`ed and the opt-in was not reverted.

**Sanity-check that the new test actually fires** (spec's last done-criterion):
with the `[lints]` block temporarily deleted from the proxy manifest,
`every_member_inherits_workspace_lints` failed naming exactly that file:

```
these members do not inherit [workspace.lints] — add

    [lints]
    workspace = true

to each of: ["crates/thegn-proxy/Cargo.toml"]
```

Manifest restored immediately after; `git diff --stat` confirmed it back to the
3-line addition.

Per the dev-loop policy and the lead addendum: no `just test`, `just lint`,
`just ci`, `just coverage`, and no e2e.

## Unverified

Things this chunk did not and could not check, for the review stage:

1. **Other consumers of `test/ratchet.sh` were not executed.** I ran the five
   bash ratchets named in the spec. The justfile invokes ratchets and chunk 2
   owns that file, so I did not read or run it; if it drives a sixth bash ratchet
   with a different name, `LC_ALL=C` is still strictly correct for it (the change
   affects only ordering, never the hit set), but it was not run here.
2. **`RATCHET_UPDATE=1` path not exercised.** Deliberate — the spec forbids
   regenerating any list. `LC_ALL=C` does change the _order in which a
   regenerated list is written_ (byte order instead of locale order), so the
   first future `just ratchet-update` may reorder lines in `test/*-ratchet.txt`
   with no content change. That is the correct order (it is what `comm` needs)
   and no list was regenerated here, but a reviewer seeing a large
   reordering-only diff on the next `ratchet-update` should know it is expected.
3. **Non-Linux / non-`en_US.UTF-8` behaviour** — the before/after was measured
   only on this Linux box under `LANG=en_US.UTF-8`. `LC_ALL=C` makes the script
   locale-independent by construction, so this should be a non-issue, but macOS
   (BSD `comm`/`sort`) was not exercised.
4. **Full-workspace effects of the two Rust/TOML edits** were not compiled.
   `cargo clippy -p thegn-proxy --all-targets` and the `thegn-core` integration
   test are the scoped gates the spec asked for; other members were not rebuilt.
   Neither edit can affect them (a new `OWNERS` row is test-only data, and
   `[lints]` is per-member), but `just ci` has not run.
5. **Pre-commit hook interaction in a shared worktree.** `prek` stashes and
   restores unstaged changes around the hook run. On `acd06415` it stashed and
   restored my own two unstaged files correctly, but this worktree is shared with
   concurrent sibling coders, so any commit here briefly stashes _their_ unstaged
   work too. Nothing was lost in this session (verified via `git status` before
   and after), but it is a race worth knowing about if a sibling reports a
   momentarily-missing edit.
