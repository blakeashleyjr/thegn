# Primary revision brief — THE-488 (round 2)

Row 548's adversarial review filed three findings. The primary's adjudication:

## F1 (P1) — ACCEPTED. This is the acceptance criterion itself. Fix it.

> `read_allowlist` accepts empty or comment-only allowlists and
> `file_ratchet_with` returns `Ok` when the current hit set is empty, violating
> the explicit fail-closed acceptance criterion.

The reviewer is right, and this is the single most important finding on this
lane: an empty hit set silently passing is precisely the "green result weaker
than the documented invariant" failure the whole issue exists to remove. A
scanner that finds nothing has almost certainly failed to scan, not proven the
codebase clean.

Required: an empty or comment-only allowlist is an **error** naming the file, and
an empty current hit set fails the ratchet rather than returning `Ok`. If a
ratchet legitimately has zero pins AND zero hits, that must be expressed
explicitly (an empty-but-intentional marker the file itself carries), not
inferred from silence. `test/help-context-ratchet.txt` is documented as
"seeded empty with no updater" — check whether it is one of these and make sure
your change does not make an intentionally-empty allowlist unrepresentable. If
it does, say so and propose the marker.

The reviewer already added
`empty_allowlist_fails_closed_even_when_no_file_hits` in all three helper
copies — keep it passing and keep the copies byte-identical.

## F2 (P1) — ACCEPTED

> `metadata` follows directory symlinks and `collect_paths` recursively descends
> them, permitting `src` symlink loops or unbounded external-tree traversal.

Accepted. Use `symlink_metadata` and **do not descend symlinked directories**.
A symlinked directory inside a scanned source root is either a mistake or a
traversal escape; either way the scanner should refuse it by name rather than
walk it. That kills the loop case without needing a depth counter or a visited
set. Refusing is consistent with fail-closed; silently skipping is not, so name
it in the error.

## F3 (P2) — NOT ACCEPTED as specified. Document it instead.

> path collection metadata and later source reads are separated by a TOCTOU
> window, so concurrent replacement can change the object scanned without an
> error.

Declined as a fix. This scanner runs inside `cargo test` over **the repository's
own working tree**, which is not an adversarial input — the only way to hit this
window is for a developer to edit a file while the test suite runs, and the
consequence is a stale scan that the next run corrects. A stable snapshot (a
tree copy) or identity revalidation would add real complexity and runtime to a
lint helper to defend against a threat that does not exist here.

Do this instead: state the limitation in the helper's doc comment — the scan is
not atomic with respect to concurrent edits, and a scan racing a working-tree
edit may reflect either version. That is honest and costs nothing.

If you can get the cheap half for free — re-`symlink_metadata` after the read and
error when len/mtime moved — you may include it, but it is optional and must not
complicate the fail-closed paths from F1/F2.

## Unchanged constraints

Keep the three helper copies byte-identical with the identity test. No
Rust-parser dependency. `profile.rs` stays reconciled by the written allowlist
reason (the reviewer confirmed it is present). No cargo/nextest/clippy/lint —
the primary runs the gate, which for this lane includes both `just lint` and
`just test`.
