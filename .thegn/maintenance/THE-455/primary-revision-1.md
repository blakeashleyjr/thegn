# Primary review — THE-455 revision 1 (reviewing row 669)

**You are right and my decision 3 was architecturally impossible.** I told you to
reuse `platform::open_directory_nofollow` / `open_cleanup_file_at`, but those are
`pub(crate)` in **thegn-host**, local ICS ingestion is in **thegn-svc**, and the
dependency runs host → svc. svc cannot reach into host, so that instruction could
not be carried out. Stopping was correct; do not work around a bad pointer.

That is the third brief of this batch a worker has corrected, and each time the
correction was right.

## DECISION — the no-follow open belongs in `thegn-core`, in `fsperm.rs`

Put it in **`crates/thegn-core/src/fsperm.rs`**, and svc calls it through
`thegn_core::fsperm`.

Three reasons this is the right home rather than a compromise:

1. **Both crates can reach it.** svc already depends on thegn-core, and so does
   host — so the same helper serves ICS ingestion now and could replace host's
   private copy later.
2. **`fsperm` is already exactly this kind of module.** Its own doc calls it "the
   0600 for secrets seam": cross-platform filesystem-permission code, `std::fs` plus
   per-OS branches, living in core. A no-follow regular-file open is the same
   category of thing, not a new one. `thegn-core` being substrate-free is about
   tokio/termwiz/HTTP/forge SDKs, not about `std::fs`.
3. **`fsperm.rs` is already pinned on `test/platform-cfg-core-ratchet.txt`**, so
   adding a per-OS branch _inside that file_ needs **no new ratchet entry**. Put it
   anywhere else in core and you will trip a shrink-only allowlist and fail the
   build.

Shape it as: open a path with no-follow and non-blocking where the platform
supports it, then **verify from the descriptor** that it is a regular file before
reading — the descriptor check is what closes the TOCTOU you identified, and it is
the reason to return a handle rather than a boolean. Return a typed error naming
what was seen (`… is a FIFO, not a regular file`), not a category.

**Do not duplicate host's private helper**, and do not inject an opener contract
from host into svc — the first is a second per-OS code path, the second is
machinery for a problem core already solves.

## Also noted from your report

Your finding that **`EventPage` on this branch is complete-or-error** is the one
that makes decision 6 safe: whole-account refusal genuinely preserves the
authoritative cache, so there is no partial-page semantics to invent. Good — say so
in the implementation's comments, because the next reader will wonder.

## Everything else from the greenlight stands

The directory-entry budget strictly larger than the eligible-file cap; filter →
stable sort → cap; symlink refusal for `.ics`; aggregate byte and file bounds
enforced before each next read; whole-account typed refusal rather than silent
truncation; directory/metadata/UTF-8/read failures as account-level errors;
cooperative deadline checks **without** building an owned worker seam; and the
deterministic-selection test with injectable candidate ordering.

Assert bounds against the **named constants**, never literals.

`hydrate_calendar.rs`: THE-459 is verified green ahead of you and removed `day_ms`
there, so rebase onto it and keep your own diff in that file minimal.

## Validation

`nix develop --command cargo check -p thegn-core -p thegn-svc --all-targets` and
`cargo nextest run -p thegn-svc calendar`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary re-runs it, plus clippy and the core ratchets.

Never report a verdict for code you could not compile; state what you could not run.
