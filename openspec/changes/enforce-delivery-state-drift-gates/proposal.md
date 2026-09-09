# Enforce delivery-state drift gates

Linear: THE-109

## Why

Linear, OpenSpec, the roadmap, generated contracts, and user documentation
diverged during the August delivery sweep. Delivered issues stayed open,
implemented changes retained unchecked task files, validation counts became
false, and plugin/control/remote documentation overstated runtime behavior.
The one-time reconciliation needs a durable, offline-safe prevention mechanism.

## What Changes

- Define a machine-readable issue/change/project delivery index.
- Add offline CI checks for orphaned active changes, stale active/archived
  references, forbidden hard-coded live validation counts, and generated
  contract/documentation drift.
- Publish a maintainer closure checklist and bounded reconciliation report.
- Keep Linear mutation a maintainer-approved action rather than an automatic CI
  side effect.

## Non-goals

- Inferring product completion solely from checkboxes or commit subjects.
- Requiring Linear credentials for ordinary builds.
- Automatically closing, reprioritizing, or rewriting external issues.
