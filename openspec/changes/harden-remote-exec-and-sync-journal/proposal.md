# Harden remote exec and the sync journal

## Why

Pull-back of a `data = "sync"` sandbox lived in one process's memory, exec could
reach the wrong Sprites account, a failed provision hid its stderr, and merge
landings did not record where they came from.

## What Changes

- A journal of successful sync uploads that any process uses to pull back.
- `sprite-exec` and `vercel-exec` route to the owning account or refuse.
- A failed Sprites one-shot exec appends the tail of stderr.
- Integrate and ready-land record provenance.
- Remote and macOS placements keep the requested OCI runtime.
- Search labels, recent lines, exact jump and grouping.

## Impact

- **Roadmap**: J (Remote access). Issues THE-155, THE-395, THE-2142.
- Affected specs: `sandbox`. No schema bump, no new config keys.
