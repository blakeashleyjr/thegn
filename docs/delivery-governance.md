# Delivery state governance

The repository owns two offline views of delivery state:
[`delivery/index.json`](../delivery/index.json) is change-centric and
[`delivery/issues.json`](../delivery/issues.json) is issue-centric. Their
schemas are [`delivery/schema.json`](../delivery/schema.json) and
[`delivery/issues-schema.json`](../delivery/issues-schema.json).
`python3 scripts/delivery_state.py validate` checks both views, their exact
back-references, and repository state. Linear remains the workflow tracker, but
an unavailable tracker must never make a source-tree gate unavailable.

## What the ledger means

Every directory directly under `openspec/changes/` has exactly one entry. An
entry records:

- lifecycle: `proposed`, `active`, `delivered-awaiting-archive`, or `archived`;
- the delivery date for `delivered-awaiting-archive`, which starts a seven-day
  reconciliation window;
- disposition: alpha delivery, deferred strategic work, a residual split,
  superseded/retirement work, or a verification-gated implementation;
- one or more Linear issue/project owners, or a reviewed rationale for having
  no tracker owner; and
- a short statement of the remaining or retirement outcome.

Separately, every reviewed active delivery issue appears in the issue
inventory with its project and sorted OpenSpec changes, or an explicit reviewed
reason why it is a tracking-only/no-spec item. This second source is deliberate:
deriving the issue list from change owners would make a newly accepted issue
with no change invisible. The gate compares both directions, so missing issue
rows, missing change rows, asymmetric links, and project disagreement fail
offline.

The validator derives the active directory set and task progress. It fails for
unmapped directories or issues, stale active entries, unknown projects, invalid
issue identifiers, completed task ledgers still called active, unchecked work
called delivered, delivery awaiting archive for more than seven days, missing
successor paths, and active references to archived changes. Neither source has a
stored total: `python3
scripts/delivery_state.py report` derives current lifecycle, disposition,
issue, project-link, and task totals.

## Normal gates

`just lint` includes `delivery-check`, the offline schema/lifecycle/ownership
gate and its negative fixtures. `just test` begins with `contract-ratchets`,
which pins the generated plugin API/support contract, generated control-v1
schema, and capability-owned surface-gap ledger. The fast validator also checks
that public plugin version, extension-point, and control-scope documentation
matches the generated plugin snapshot.

Failures name the owning capability or active change. Update a generated
snapshot only after making an explicit compatibility decision; do not weaken a
ratchet merely to make a gate green.

## Read-only tracker reconciliation

The default validator and report do not import an HTTP client, inspect tracker
credentials, or contact Linear. To compare repository ownership with a
separately exported read-only issue list, run:

```sh
python3 scripts/delivery_state.py report \
  --linear-json /path/to/read-only-linear-issues.json
```

The export may be either an issue array or `{ "issues": [...] }`. Each issue
needs `identifier` (or an `id` containing `THE-…`), `project`, and `status`.
Project mismatches, missing issues,
terminal issues still called active, and active issues in a delivery project
that are absent from the checked-in issue inventory are reported as drift. This
command never closes, edits, or reprioritizes an issue. Output is bounded to the
first 100 drift details while preserving the full count and a truncation marker
in JSON.

## Maintainer closure checklist

Before closing an issue or archiving its OpenSpec change:

1. Re-read the issue acceptance criteria, proposal, design, delta specs, and
   task ledger; separate residual work instead of erasing it.
2. Cite the implementation, tests, and any required real-platform/live-service
   evidence. Check only tasks supported by that evidence.
3. Run strict validation for the affected OpenSpec change and the focused tests
   named by its acceptance criteria.
4. Run `just delivery-check` and the relevant generated-contract/surface
   ratchets. Run the normal repository gate required by the change.
5. Set the ledger lifecycle to `delivered-awaiting-archive` and record
   `delivered_at` only when no required task remains. Archive within seven days.
   Verification-gated work stays active until its named environment has actually
   been exercised.
6. Archive through the normal OpenSpec workflow, then update or remove the
   change entry and active-issue entry in the same reviewed change. Preserve
   every still-shared issue/change link. Never leave an archived directory
   referenced as in-flight work or a closed issue in the active inventory.
7. Re-run `just delivery-check` after the archive.
8. Only then apply the separately authorized Linear transition and add a concise
   reconciliation comment with code/spec/test evidence and links to any residual
   issues. CI and this script never mutate Linear.

For a shared active change, closing one delivered issue removes only that issue's
owner and issue-inventory row. The change remains active with every remaining
owner and back-reference intact; it is archived only when its full accepted scope
is delivered. This is the required transition for THE-131 while the remaining SCM
customization issues stay open.

Superseded proposals are not implemented merely to empty the directory. Point
them at their canonical successor, reconcile any accepted historical behavior,
and archive them through the same checklist.
