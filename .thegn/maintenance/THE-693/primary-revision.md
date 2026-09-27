# Primary revision brief — THE-693 (round 2)

Row 619's review found two HIGH defects. **Both are accepted. F1 must be fixed
properly; F2 must be narrowed and the residual documented.** The rest of the
implementation stands — the authority, the byte validation, the typed refusals and
the call ordering in `remove_checked` are all the approved design and are good.

Before anything else: the primary already fixed two build failures on this branch
(`15a353a9`) — `render_managed` needed its `thegn_core::skills::` path, and the
now-fixture-only `observe_status` / `clean` / `Verified::probe` wrappers are gated
behind `#[cfg(test)]`. Merge/keep those; do not revert them.

## F1 — fix it properly. This is a data-loss path, not a nit.

The reviewer is right and the asymmetry is stark:

- `skill_seed.rs:79-123` adds **both** the raw and marker-wrapped MQ command bytes
  to the authority, ignoring `cfg.skills.exclude`.
- The writer at `skill_seed.rs:746-766` does the opposite: when `mq` is excluded it
  treats the command as **user state**, and removes it only if `inspect_managed`
  proves an existing marker.

So with `skills.exclude = ["mq"]` in a repo that does not ignore the path, a user's
own `mq-add.md` containing those exact bytes is accepted as tool state and deleted
by an automatic sweep — a file the configured writer would never have produced and
would never remove. The reviewer's regression at
`merge_cleanup_tests.rs:511-533` is expected red; make it green by fixing the
authority, not by changing the test.

**Derive the authority from the effective seeding plan**, so it can only ever claim
what this configuration would actually write. Do not special-case `mq` — if the
authority is computed from the same plan the writer executes, exclusion is handled
by construction and the next excluded skill cannot reintroduce this. That is the
single-authority requirement from the original greenlight, and F1 is what its
absence costs.

Keep the conservative half the writer already has: an unmarked file at a managed
path stays protected.

## F2 — narrow it, prove the narrowing, and write the residual down

Accepted as real: validate-then-unlink-by-pathname means a replacement can be
deleted, and unlike every other case in this chain, git's no-force removal cannot
back it up, because we did the deleting.

Do **not** resolve this by switching to `git worktree remove --force`. Git's
independent refusal is the second of the two protections this whole chain relies
on, and trading it away to dodge a race would be a bad bargain.

Narrow it instead:

1. Pin the parent directory once (open it `O_DIRECTORY | O_NOFOLLOW`) and do the
   validating open and the unlink **relative to that same descriptor**, so the
   directory cannot be swapped underneath either step. Use or extend the existing
   `platform::` seam — `open_nofollow` is already there; anything new is per-OS and
   belongs in `src/platform/`, not at this call site (the platform-cfg ratchet will
   tell you if you get that wrong).
2. Capture the validated file's identity (device + inode, and link count) from the
   open handle, then re-verify it immediately before unlinking. A mismatch is a
   refusal, not a deletion.
3. **Write a deterministic test for the mismatch path.** A race is hard to test, an
   identity mismatch is not: swap the file through a seam between validation and
   unlink and assert the refusal fires and the replacement survives. "Not covered by
   a deterministic regression test" was part of the finding; answer that part.
4. Document the residual window in the code, in the same voice as the existing
   comment at `merge_cleanup.rs:1020-1032`, and say plainly what it is: a same-UID
   process replacing an exact-match file inside the remaining window. `gate_path.rs`
   already sets the precedent — "advisory locking is not a filesystem lease against
   an arbitrary hostile process with the same UID." Be that honest.

If after narrowing you judge a residual window still exists that cannot be closed
without a filesystem lease, say so explicitly in the report rather than implying it
is gone.

## Unchanged requirements

- A worktree with genuine tracked modifications or genuine untracked user files is
  never swept, with or without `--force`.
- Seeded content **plus** real user work is never swept; all records are classified
  before the tree is accepted.
- A managed path whose **content differs** is user work.
- Unknown generated paths (including OpenSpec/dev-shell output) stay protected and
  out of the authority.
- No refusal may report thegn's own writes as "edited since landing".
- The full test matrix, each case with **and without** `--force`.

## Validation

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_cleanup`. **The sandbox mounts
`/nix/store` read-only, so this usually fails** — say exactly that and stop if so.
The primary runs clippy, the full workspace nextest and smoke centrally, and has
already caught two build failures on this branch plus one on each previous lane in
this chain. Report honestly rather than optimistically; `implementation-ready` for
code you could not compile is the one outcome that costs a whole round.
