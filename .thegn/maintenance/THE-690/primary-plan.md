# Primary review + greenlight — THE-690

Reviewing row 611's investigation. **APPROVED. Implement the five-step fix as
written, with the two escalated decisions settled below and one adjustment to the
timeout.**

Good escalations. Both questions were the right ones to ask rather than assume,
and your instinct on each — fail closed for unknown dialects, never depend on the
developer machine's ambient runtime — matches the decision.

## Decision 1 — support Docker and Podman; fail closed for every other OCI kind

The primary enumerated the OCI family from the profile table. It is:

| backend           | binary         | status                                               |
| ----------------- | -------------- | ---------------------------------------------------- |
| Podman (rootless) | `podman`       | supported                                            |
| PodmanRootful     | `podman`       | supported — **same binary**, so dedup as you planned |
| Docker            | `docker`       | supported                                            |
| Smol              | `smolmachines` | `EventsCap::Reserved("unverified runtime")`          |
| Apple             | `container`    | reserved                                             |
| Wsl               | `wsl.exe`      | reserved                                             |

Implement the query for `docker` and `podman` only — their `ps -a -q` plus a
mount-source `inspect` format are the same dialect, so that is one implementation,
not two.

**Any other discovered OCI candidate refuses as "could not be queried."** Do not
silently treat it as absent. Reasons, in order:

- CLAUDE.md's seam rule is implemented-or-`reserved`, and `Smol`/`Wsl` are
  explicitly unverified. Treating an unverified runtime as proof of _no_ ownership
  is exactly the inversion this issue is about, just narrower.
- This guard authorises deleting a directory. Fail-closed is the correct default
  for every genuine unknown.
- None of those three binaries exists on the affected machine, so supporting
  Docker and Podman unblocks the real case without weakening anything.

Note `container` is a generic name; if something unrelated called `container` is
on PATH the sweep will refuse with "could not be queried". That is acceptable and
diagnosable **provided the message names the binary it could not query** — include
it.

## Decision 2 — smoke gets a deterministic fake runtime, and keeps the no-runtime case

Do **not** settle for the no-runtime branch. The whole claim of this issue is _an
installed runtime must not block the sweep_, and the no-runtime path cannot
demonstrate that.

Add a shim on the fixture PATH — a small script named `docker` (and/or `podman`)
that prints a canned, deterministic response — and cover both outcomes end to end:

1. shim reports a container whose mounts are **unrelated** ⇒ the worktree **is
   swept**. This is the case that reproduces Blake's bug and is the point of the
   whole change.
2. shim reports a container whose mount source **is the worktree** (or a
   subdirectory of it) ⇒ **kept**, with and without `--force`.
3. shim exits non-zero / hangs past the timeout / prints garbage ⇒ **kept**, with
   the could-not-be-queried reason.
4. no runtime on PATH at all ⇒ unchanged behaviour (keep the existing minimal
   `git`+`sh` PATH case for this).

That is hermetic, deterministic, and independent of what is installed on the host
— which is the coupling this issue exists to remove. Replace the existing
workaround comment with one that says the minimal PATH now tests the _no-runtime
branch deliberately_, rather than dodging a bug.

## Adjustment — do not make the timeout the new permanent blocker

Your "explicit sub-two-second timeout" is too tight. A cold Docker CLI against a
healthy-but-busy daemon can exceed two seconds, and because the guard fails
closed, a too-short timeout recreates the exact symptom this issue fixes — a
sweep that never collects — just non-deterministically, which is worse to
diagnose.

Use a budget that will not fire on a healthy daemon (**5 seconds per runtime** is
the primary's call), keep the output cap at or below the existing 16 KiB policy,
and make the timeout refusal say **that it timed out** and after how long. Bounded
is the requirement; tight is not.

## Confirmed as written

Steps 1–5 stand, including: keeping the existing bounded PATH validation;
deduplicating the two `podman` entries; one `ps -a -q` plus **one** batched
`inspect` per runtime rather than a process per container; rejecting truncated or
malformed output rather than reading it as empty; and leaving `--force` alone so
the same admission guard runs in both modes.

Your canonicalisation rule is exactly right and is the subtle part: canonicalise
the target and each nonempty mount source, match on equality **or** at/under the
target as a path-component comparison (not a string prefix — `/a/bc` must not
match `/a/b`), and treat a source that cannot be canonicalised as a match, so a
symlinked or relative source can never slip past as unrelated.

Keep the change at the `merge_cleanup` admission boundary. Do not add a core API
for the mount-source inspect builder; vendor command construction stays inside the
probe, per the seam rule that vendor CLIs live only in their implementation file.

## Out of scope

- **THE-689** — the session-admission-lock and ghost-tab races. Filed, accepted,
  deliberately deferred. Do not absorb them.
- The four landed fixes in this chain: THE-685's filter drivers, THE-686's
  `StatusObservation` / `Refusal::Changed`, THE-687's `derive_landed_commit` and
  landed-OID guards, THE-688's layout teardown. Preserve; do not refactor.
- `has_cleanup_tenancy` / `has_cleanup_dispatch`, the submodule and special-index
  guards, TTL arithmetic, branch-retention holds.

## Report the pattern

You correctly identified that this is the third instance of one shape — THE-687
(absent cache field), THE-688 (saved layout row), THE-690 (installed binary), each
substituting a cheap proxy for the question actually being asked, each producing a
_permanent_ refusal. If you find a fourth while you are in here, **note it; do not
fix it.**

## Validation

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_cleanup`. **The pipeline sandbox
mounts `/nix/store` read-only, so this usually fails outright** — if it does, say
exactly that and stop. The primary runs clippy, the full nextest workspace and
smoke centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every previous lane in this chain shipped code that did not compile
— an ambiguous `Vec::new()`, a by-value row where a reference was wanted, a
`PathBuf` that was never imported — and the primary caught each one. That is the
expected division of labour, not a failing, so report honestly.
