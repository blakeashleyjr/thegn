# Primary decision — THE-677 (unblocks row 623)

**Your refusal was correct and I am not overriding it.** You were asked for a
never-delete-live guarantee, you found that mtime, lockfile membership and
same-name-newer cannot establish it, and you stopped instead of shipping a
heuristic dressed as a guarantee. That is exactly right: a reclaimer that
silently evicts a live artifact would turn one wrong guess into a surprise
workspace rebuild, and nobody would attribute it to thegn.

So the primary's answer is not a liveness oracle — **there is none available
here, and we are not inventing one.** The answer is to change the guarantee to
one that is actually achievable, and to take the one piece of real authority that
does exist.

## DECISION 1 — there is no off-cargo liveness authority. Stop looking for one.

Cargo decides freshness by recomputing a unit hash from the current lockfile,
feature set, profile and rustc version, then looking for
`target/<profile>/.fingerprint/<pkg>-<hash>/`. That hash is cargo-internal and
not reproducible from outside cargo, and the only way to make cargo tell us is to
run a build — which this path cannot do: the background disk scan must never run
cargo, and requiring a build before a prune is circular.

**Do not add a cargo invocation to make the guarantee true.** Record this
conclusion in the design rather than leaving the next reader to re-derive it.

## DECISION 2 — the guarantee changes: bounded cost, not never-wrong

Replace "never deletes a live artifact" with a guarantee we can hold:

> The reclaimer never removes the newest generation of any build unit, and the
> worst case of a wrong eviction is recompiling that one unit — never a
> workspace rebuild.

Concretely, the policy keeps the **newest K generations per build unit** and
evicts older ones. Defaults:

- **`K = 3`.** This box legitimately holds several live generations of the same
  unit at once — the host build, a cross triple, and the coverage
  instrumentation each produce their own hash. Three covers the normal case; the
  key is configurable for anyone whose matrix is wider.
- **A minimum age floor** (propose `24h`, one key, documented). Nothing recently
  built is a candidate however many generations exist, so an in-flight matrix
  build is not pruned out from under itself.
- **Never the newest**, unconditionally, regardless of K or age.

This is a real change to the issue's wording, so **state it plainly in the
design** and say why: the stated guarantee was unachievable without running
cargo, and a weaker guarantee that is actually true is worth more than a strong
one that is enforced by a heuristic.

## DECISION 3 — synchronization with external cargo builds IS authoritative: take cargo's lock

This half of your question has a real answer. Cargo holds an **exclusive flock on
`target/<profile>/.cargo-lock` for the entire duration of a build** — this repo
already documents and relies on that fact (it is why `[disk] shared_target_dir`
is deliberately not the default: one shared target dir would serialize every
worktree's builds).

So, per profile directory:

1. Try to acquire that flock **non-blocking**.
2. **Failure ⇒ skip this profile entirely, this scan.** A held lock means a build
   is running; it is not a reason to wait and certainly not a reason to proceed.
3. Hold it for the prune, so a build cannot start midway through.

That gives exactly the synchronization you said was missing, at the cost of one
`flock` attempt. **Never block on it** — this runs on the background scan and a
blocking acquire would sit behind a 20-minute compile.

## DECISION 4 — group by `.fingerprint/`, delete a unit's whole footprint

Do not parse `deps/<name>-<hash>.<ext>` filenames as the unit inventory. Use
`target/<profile>/.fingerprint/<pkg>-<hash>/` as the enumeration: it is cargo's
own per-unit record and gives an exact unit-to-hash mapping.

Evicting a generation must then remove **that unit's whole footprint together** —
its `deps/` artifacts, its `.fingerprint/` directory, and its `incremental/`
entry if present. Half-removing a generation leaves cargo to discover an artifact
with no fingerprint, and the failure mode of that is worse than the disk it
saved. If any part of a footprint cannot be removed, **leave the rest** and
report it.

## DECISION 5 — measure generations, not whole trees

The issue wants a generation-level number and today only a whole-`target/` byte
count exists. Report **bytes reclaimed per profile from generation eviction**,
separately from the existing whole-`target/` reclaim, so the two are never
conflated in a notification. A scan that pruned nothing must say so rather than
reporting the whole-tree figure.

## Where the code goes

- **Policy is pure and lives in `thegn_core::disk_reclaim`**, beside the existing
  idle/low-disk policy, with unit tests. `thegn-core` is substrate-free and gated
  at 95% lines, so the decision function takes an inventory and returns evictions
  — it does not touch the filesystem.
- **All I/O is in the host**, at the tail of the background disk scan where the
  existing reclaim already runs. Never on the event loop, never before the first
  frame.
- New `[disk]` keys (K and the age floor) trip **three** ratchets: the
  `config.toml.example` key-coverage check, the env-overlay ratchet, and strict
  config validation. Budget for all three.

## Next step for this lane

This is a `maintenance-plan-revision`, not a code dispatch: your investigation
ended blocked and never produced an ordered implementation plan, and the
guarantee just changed. Produce the ordered chunks against the five decisions
above — including the exact `[disk]` key names you propose and the inventory
type the pure policy will take — and flag anything in them you think is wrong.
Disagreement with a primary decision is a finding, not insubordination.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report a verdict for code you could not compile; state what you could not
run.
