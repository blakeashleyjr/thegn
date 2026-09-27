# Primary greenlight — THE-677 (reviewing row 630, plan-ready)

Chunks 1–4 are approved as written. **All three of your findings are correct, and
two of them overturn decisions I made. Thank you for pushing back — that is what
this stage is for.** Each is answered below, and the answer to finding 1 changes
the policy, so read that first.

## Finding 1 — you are right, and the unit key is abandoned

You are right that "newest K per unit" has no meaning until a build unit is
defined, and that both available definitions fail: package-name grouping
collapses distinct target/profile/feature/build-script units, while treating each
fingerprint ID as its own unit reaps nothing. You are also right that parsing
private Cargo JSON to recover the real key is not acceptable.

**So drop the unit key. The policy no longer needs one.**

Replace "newest K generations per unit" with:

> A generation footprint is eligible only when nothing in it has been touched for
> at least the configured age floor, and it is never the newest footprint for its
> package name.

The age floor does the work, and it is **not** a new heuristic being smuggled in —
it is the threshold this repo already accepts. `[disk] idle_clean_days = 14`
reclaims an entire `target/` tree after two untouched weeks. Evicting a single
build generation untouched for the same period is **strictly more conservative
than what thegn already does today**, so it needs no stronger justification than
the existing policy has.

Concretely:

- **Default the age floor to 14 days**, explicitly documented as matching
  `idle_clean_days`. Not the 24h I proposed — 24h was chosen to make K safe, and K
  is gone.
- **Keep a cheap newest-per-package-name belt**: never evict the most recently
  touched footprint for a package name, whatever its age. This costs nothing and
  removes the pathological case where a package legitimately untouched for months
  loses its only generation.
- **K is removed from the design.** One new key, not two: the age floor. That is
  one `section.key` and its three ratchets (config example, env overlay, strict
  validation), so chunk 2 shrinks accordingly — drop the K field, its default, its
  overlay, its env mapping and its zero-validation.

Chunk 1's regression list adapts cleanly: keep the age-floor cases (younger than
the floor is retained; exactly at the floor is eligible; future timestamps
saturate to age zero; empty inventory is a no-op; units never affect each other;
reported bytes equal selected bytes) and drop the K cases.

## Finding 2 — you are right, the cost bound was overstated

I claimed "at most one unit recompiles; never a workspace rebuild." You correctly
observe that a mistaken footprint association can make Cargo rebuild dependents
and test binaries too. **Narrow the claim exactly as you propose**, to the
retention invariant alone:

> The reclaimer never removes a footprint that has been touched within the age
> floor, and never the newest footprint for a package name. It makes no claim
> about how much rebuilding a wrong eviction causes.

Say that in the design and in the documentation, and do not restate the stronger
version anywhere. Overstating a safety property is worse than not having it,
because it stops the next reader from checking.

## Finding 3 — you are right, and deletion ORDER is the answer

Sequential unlinking cannot roll back, so "if any part cannot be removed, leave
the rest" was not implementable as written. Do not build a staging protocol for
this. **Order the deletion so that every partial state is a safe one:**

1. **Remove the `.fingerprint/<pkg>-<hash>/` directory FIRST.**
2. Then the `deps/` artifacts for that hash.
3. Then the `incremental/` entry.

A crash or failure after step 1 leaves artifacts with no fingerprint: Cargo
considers the unit not fresh and rebuilds it. That is a wasted rebuild, which is
the cost we already accept.

The reverse order is the one that must never happen: artifacts deleted while the
fingerprint survives leaves Cargo believing a unit is fresh when its output is
gone. **Write that reasoning as a comment at the deletion site**, because the
ordering looks arbitrary and someone will otherwise "tidy" it.

Then report partial deletion accurately — planned bytes vs actually removed bytes,
per profile, with failure counts — and **do not describe best-effort sequential
deletion as all-or-nothing**, exactly as you say.

## Confirmed, and the part that must not soften

- **Non-blocking `.cargo-lock` acquisition per profile; a failed lock skips that
  profile for the round.** Never block — this runs on the background scan and a
  blocking acquire would sit behind a 20-minute compile. Hold the lock through
  pruning.
- **Never walk profiles unrelated to the fresh scan**, never run Cargo, never on
  the event loop, never before the first frame.
- Pure policy in `thegn_core::disk_reclaim` (substrate-free, 95% line gate), all
  I/O in the host at the existing scan tail.
- Generation-reclaimed bytes stay **separate** from the whole-`target/` figure, and
  zero eviction reports zero rather than reusing the worktree number.
- Do not weaken or bypass the example/env coverage gates to make a ratchet pass.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report a verdict for code you could not compile; state what you could not
run. If a further finding contradicts a decision above, say so — the last round
proved that worth doing.
