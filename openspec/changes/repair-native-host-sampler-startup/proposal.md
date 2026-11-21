# Repair native host sampler startup

## Summary

Defer constructing Observe's native host sampler until its worker observes an
active source. Sampler construction performs platform probes, including the
Apple HID temperature probe, so a source hidden immediately after creation
must be able to park without first completing hardware discovery. Keep real
sampling on its dedicated worker, preserve the discarded CPU priming read on
resume, and retain wakeable bounded stop behavior.

## Impact

- **Observe host source** — defer platform sampler construction until activation.
- **Lifecycle proof** — use an injected deterministic sampler for host-source
  tests; retain assertions for first publication, pause/resume, and prompt exit.
- **Delivery** — THE-2566 owns native macOS repeated serial/concurrent tests and
  a real sample witness on build-mac; this worktree does not run local builds or
  tests.

## Non-goals

- Changing the host metrics collected, sampling cadence, shared Observe history,
  source activation contract, or user-visible dashboard behavior.
- Running or provisioning a native macOS worker from this worktree.
