# Design — project vocabulary compatibility

## Vocabulary map

| Concept                       | Canonical product term  | Compatibility/internal term |
| ----------------------------- | ----------------------- | --------------------------- |
| one repo shown in the sidebar | project                 | workspace                   |
| named multi-repo group        | program                 | project                     |
| provider-owned account scope  | provider-qualified term | unchanged fields            |

## Compatibility policy

Core normalizes legacy config spellings before deserialization, reports a
precise deprecation/duplicate diagnostic, and makes the canonical value win.
The compatibility window is three stable releases with a named removal release.
Canonical writers, schemas, the example, and Home Manager emit project forms;
legacy Home Manager/config/environment forms remain accepted during the window.

The host similarly recognizes exact old CLI/action forms at the edge and warns
without contaminating JSON. Palette/help search retains old vocabulary.

## CLI and capability collision

The pre-existing multi-repo `thegn project` namespace becomes `thegn program`;
the old command and `--project` flag remain behavior-identical warned aliases.
No new one-repo `thegn project` command is invented.

The capability catalog exposes canonical `program.*` entries and deprecated
`project.*` aliases with identical verb, scope, and projected surface policy.
Lookup accepts both; canonical-by-verb selection and coverage do not double-
count aliases.

## Stable machine/state boundary

No DB table/column, migration, internal `Workspace*` type, existing JSON field,
tracker `workspace_id`/`workspace_slug`/`project_id`, Cargo construct, or
container path is renamed. This is a product-language and bounded compatibility
change, not a storage rewrite.

## Verification

Pure config normalization and catalog/action alias tables are unit-tested.
Schema/help/config tests pin canonical output and compatibility. The accepted
design did not require running a built binary against live state.
