# Primary review — THE-407 revision 1 (reviewing row 670)

The schemars fix and the registry work are fine. **The two failing tests are
correct and the implementation is wrong** — do not update the assertions.

## The defect: one constant, two meanings

```
FAIL config::tests::app_tab_config_defaults_to_work_first_and_default
  assert_eq!(cfg.apps.effective_tab_order(), vec!["work"]);
    left:  ["work", "observe"]
    right: ["work"]
```

`BUILTIN_TABS` went from `["work"]` to `["work", "observe"]`, and
`effective_tab_order()` derives from it — so **every existing user gains an
`observe` tab on upgrade** without configuring anything. That is a silent
user-visible default change, and it is not what this issue asked for.

The issue is that `observe` cannot be _ordered or selected_. Fixing that requires
`observe` to be **known**. It does not require it to be **enabled**.

## DECISION — split the two concepts

- **`BUILTIN_TABS` = the known ids.** Includes `observe`. This is what `[apps]`
  validates against and what the "unknown app tab; known tabs are …" message
  lists. Growing it is the actual fix.
- **`effective_tab_order()`'s default stays `["work"]`.** A tab appears because
  `[apps]` names it, not because the binary knows about it. Someone who never
  edits config sees exactly what they saw before.

Concretely: `effective_tab_order()` must not fall back to "all known tabs". Its
default is an explicit default list — `["work"]` — and `BUILTIN_TABS` is only the
membership test applied to whatever the user wrote.

Both failing tests then pass unchanged, which is the signal that the split is
right. Add one more: `[apps]` naming `observe` **does** produce
`["work", "observe"]` (or the configured order) — that is the feature, and nothing
currently proves it.

The third failure,
`config_admission::tests::cli_schema_is_checked_before_lenient_override_deserialization`,
is almost certainly the same root cause reaching the schema; re-check it after the
split rather than treating it separately.

## Why this matters beyond this lane

This is the third instance in two batches of **one name carrying two meanings**:
two MIME guards that disagreed about a content type, a window represented as both
dates and instants, and now "known tabs" doing duty as "enabled tabs". Each time
the fix was to separate the concepts rather than reconcile the values. Worth
naming in the code comment so the next reader sees why `BUILTIN_TABS` and the
default order are deliberately not the same list.

## Keep as implemented

The `AppTabId` schema (with schemars 0.8's `is_referenceable() -> false`, already
fixed by the primary), the registry projecting from core rather than the reverse,
the live-reload `AppHost` rebuild, and the unknown-tab validation message naming the
known ids.

## Validation

`nix develop --command cargo nextest run -p thegn-core -p thegn-host` — the **whole**
crates. A scoped filter is what let all three of these reach the land gate. **The
pipeline sandbox mounts `/nix/store` read-only, so this usually fails outright** —
say exactly that and stop if it does; a supervisor runs the crate-wide verification
centrally.

Never report a verdict for code you could not compile; state what you could not run.
