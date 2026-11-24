# Vercel selected runner (first slice)

Linear: THE-2418

## Why

A project can already select a Sprite to run pinned build and test Jobs. The
same route needs a Vercel Sandbox executor that reuses the Rust Vercel provider
(team and project binding, file transfer, command API, reaper) and keeps the
developer's agent, model login and provider token local.

## What changes

- Add `thegn_svc::vercel::job`: a `SelectedRunner` implementation that runs one
  pinned `cargo build` or `cargo test` attempt on a fresh, owned, per-attempt
  sandbox with an admitted vCPU ceiling and a bounded lifetime.
- Add a durable per-project attempt ledger that records the sandbox name before
  the create request and the sandbox, session and command handles before a
  result is trusted, and pins the ledger to one Vercel team and project.
- Reuse the Sprites attempt state, cleanup and resume types and the snapshot
  capture and digest code instead of duplicating the contract.
- Split `VercelProvider::create` into `create_named` so a job attempt can pass
  its own sandbox name and a deadline-derived timeout; `create` keeps its
  behavior.
- Document the executor in `docs/help/vercel-sandbox.md`.

## Impact

Crate `thegn-svc` only, plus help text. Fixture-tested against a loopback
server; never run against Vercel. Out of scope for this slice and still open on
THE-2418: wiring a project's runner choice and configuration to the executor,
live runs, job-sandbox usage and cost reporting through the usage tracker, and
a listing-based check of the checkout (the read-back of every file plus a
fresh sandbox stands in for it).
