# Primary review + greenlight — THE-191

Reviewing row 584's investigation. **APPROVED, with the scope narrowed below.**

Your evidence is good and the important part is the structural finding: live
crash/backoff state is **loop-local** in the compositor
(`handlers/plugins.rs:27-44`, `:475-515`), the supervisor has no health snapshot
(`thegn-svc/src/plugin/session.rs:56-105`), and `plugin list` is
config/discovery-only (`cmd/plugin.rs:99-164`).

## Your question — do NOT build a new control boundary

You asked the primary to choose an authoritative compositor-to-CLI/control
attachment boundary, since daemon control does not own the plugin runtime.

**Answer: none. That is out of scope for this lane.** Publishing compositor
loop-local state onto the control surface is a new plumbing seam with its own
ownership, lifetime and generation questions — a change of a different size from
"make `plugin list` truthful".

What this lane delivers instead, which still fixes the stated complaint:

1. **Report authoritatively what a CLI process can actually know** —
   config/discovery state, plus any health the supervisor already persists. If
   the supervisor persists nothing today, say so rather than inventing a store.
2. **Never imply health you cannot observe.** When live runtime state is
   unreachable from a CLI process, the output must say exactly that — e.g. a
   `live health unavailable: no attached compositor` line, and the machine-
   readable form carries the same distinction. Today's output is misleading
   because absence reads as healthy; making the absence explicit is the fix.
3. **Define the typed state vocabulary now**, even where some variants are not
   yet observable from the CLI: `disabled-by-config`, `starting`, `healthy`,
   `degraded`, `crash-disabled`, `stopped`, `unknown` (not observable here).
   One enum, one serialization, both output modes. That way the follow-up that
   plumbs live health fills in variants rather than redesigning the contract.

Record the compositor→control health-publication seam as an explicit follow-up
finding, naming `handlers/plugins.rs` and `plugin/session.rs`.

## Restated

- `stale-generation` stays a follow-up: THE-165 is unlanded, so there is no
  generation to report. Do not invent one.
- Bound and strip control characters from any last-failure text — plugin stderr
  is untrusted. Reuse the existing bounded-display helper; do not write a third.
- Human and JSON must agree on state names and exit semantics.

## Tests

Assert each reachable state renders with its stable name in both output modes;
assert unreachable live state renders as explicitly unknown, never as healthy;
assert failure text is bounded and control-free.

## Validation

`cargo check -p thegn-host --all-targets` and a focused `cargo nextest run -p
thegn-host plugin`. **If `nix develop` fails inside your sandbox** — the pipeline
env mounts `/nix/store` read-only, which breaks it — say so and stop; the primary
runs them. Do not claim ready without stating which you actually ran.
