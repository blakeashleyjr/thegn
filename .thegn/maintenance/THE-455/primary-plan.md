# Primary review + greenlight — THE-455

Reviewing row 664. **APPROVED — this is the strongest plan of the batch, and it
caught four things my brief did not. The open questions are decided below.**

What you found that I had not asked for:

- **The scan-entry budget must be strictly larger than the eligible-file cap.**
  That is the insight. Capping the raw iterator at the ICS cap is the bug in a new
  costume — a directory with many unrelated entries would then fail even though it
  holds few `.ics` files.
- **The TOCTOU between enumeration and open** — a candidate replaced or no longer a
  regular file between the two.
- **A no-follow/nonblocking open is what actually prevents the FIFO hang**, before
  any deadline can fire. The deadline is defence in depth, not the fix.
- **No partial-page semantics.** A bounded scan must fail the fetch and preserve the
  previous authoritative cache, not install a truncated snapshot.

## DECISION 1 — the directory-entry budget is approved

Add it as a separately named limit, strictly greater than the eligible-file cap,
with its own typed refusal. Do **not** cap the raw iterator at the eligible-file
cap.

## DECISION 2 — symlink policy: require a regular file, refuse `.ics` symlinks

Take your recommended default. A calendar directory is **data**, and following a
symlink out of it is a path escape that the operator did not ask for; refusing is
the conservative reading and it matches this repo's posture elsewhere. Apply it to
the configured single-file source **and** vdir children, and test it.

## DECISION 3 — reuse the existing no-follow platform seam, do not add one

You correctly flagged the cfg ratchet. There is already a seam for exactly this:
`crate::platform::open_directory_nofollow` and `platform::open_cleanup_file_at` /
`cleanup_file_identity`, added for merge cleanup, which open relative to a pinned
parent and report device/inode/link identity. **Read those first and reuse them.**
If they genuinely do not fit a calendar source, say why — but do not create a
second per-OS open helper, and do not add a platform `#[cfg]` outside
`src/platform/` (the ratchet is shrink-only and will fail the build).

Those same helpers give you the TOCTOU check for free: capture identity at
enumeration, re-verify after open.

## DECISION 4 — cooperative deadline yes, an owned worker seam NO

Add the cooperative checks between enumeration, reads and parser work. **Do not
build an owned worker seam** for a hard wall-clock guarantee. You are right that a
Tokio timeout cannot cancel synchronous work — so say that plainly in a comment
rather than implying a bound you do not have. If you conclude a hard guarantee is
genuinely required, report it as a finding; it is a separate change.

## DECISION 5 — on THE-452, treat the branch as authoritative

I have not verified THE-452's state, so do not assume it. The page contract **as it
exists on your branch** is the contract. If the code looks like THE-452 already
changed it, stop and report that rather than integrating with a contract you
inferred. And do not build a competing status model either way.

## Confirmed as written

Named limits for eligible files and total source bytes, enforced **before** reading
each next document; whole-account refusal with a typed reason on any exceeded cap,
never silent truncation; a deterministic selection helper with **injectable
candidate ordering** so the test does not depend on readdir order; filter → stable
sort → cap, in that order; directory/metadata/UTF-8/read failures as account-level
errors rather than debug-only skips.

Align the byte ceiling with the existing account/global reservation policy, or
document why a separate cap is needed — your call, but state which you chose.

## File coordination

`hydrate_calendar.rs` — your "ideally no change" is the right target. **THE-459 is
landing first** and is deleting `day_ms` from that file. If you must touch it, keep
it to `sync_accounts_with_generation` and name the function in your report.

Assert bounds against the **named constants**, never literals: a hard-coded bound
rots the moment the constant moves, which cost a round last batch.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the crate-wide nextest and the ratchets
centrally.

Never report a verdict for code you could not compile; state what you could not
run.
