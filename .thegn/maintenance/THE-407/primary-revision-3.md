# Primary review — THE-407 revision 3 (reviewing row 677)

**Stop. The code is right and my ruling was wrong.** I traced it this time instead
of writing another brief, and the conflict is between two things I said, not
anything you did.

## What is actually happening

The panic is `registry.rs:83`, which is **not** the opt-in assertion — it is the
last one:

```rust
assert_eq!(enabled(&cfg).count(), 0, "apps are opt-in");   // line 77 — PASSES
cfg.observe.enabled = true;
assert_eq!(ids, ["observe"]);                              // line 80 — PASSES
let host = AppHost::from_config(&cfg);
assert!(labels.iter().any(|l| l.contains("Observe")));     // line 83 — FAILS
```

So everything I asked for in revisions 1 and 2 is implemented and working:
`observe.enabled` defaults to `false` ("Off by default" in `config_observe.rs`),
`enabled: |cfg| cfg.observe.enabled` gates the builder, and `enabled()` filters on
it. Apps _are_ opt-in.

The failure is that an app which **is** enabled still gets no tab, because
`AppHost::from_config` builds from `cfg.apps.effective_tab_order()` and I told you
that must default to a fixed `["work"]`.

## DECISION — the default order is DERIVED, not a literal

That was the mistake. `effective_tab_order()` is **`["work"]` plus the enabled
apps**, in `[apps]` order where the user specifies one.

This satisfies both constraints I was trying to hold, which is why I should have
seen it two rounds ago:

- **Nobody's UI changes on upgrade.** `observe.enabled` is `false` by default, so
  the derived order _is_ `["work"]`. `app_tab_config_defaults_to_work_first_and_default`
  keeps passing unchanged — check that it does.
- **Enabling an app is the opt-in.** Setting `observe.enabled = true` is the
  configuration act. Requiring the user to _also_ list it under `[apps]` would mean
  that flag did nothing, which is indefensible.

`[apps]` therefore orders among `{work} ∪ {enabled apps}`; it does not decide
membership. Naming a **disabled** app there is a harmless no-op, not an error.
Naming an id absent from `BUILTIN_TABS` is still an error listing the known ids.

`config_admission::cli_schema_is_checked_before_lenient_override_deserialization`
should follow from the same change; if it does not, report what it actually asserts
rather than adjusting it.

## What I got wrong, recorded so the comment is accurate

Revision 1 said "the default order stays `["work"]`", and I meant "nobody's tabs
change". Those are only the same sentence while no app is enabled — I wrote down the
_value_ instead of the _rule_ that produces it, and then spent revision 2 adding a
third concept to prop up the wrong mechanism.

So the comment I asked for at `BUILTIN_TABS` should name three questions and the one
derivation:

1. **`BUILTIN_TABS`** — which ids exist (validation, and the known-ids error text).
2. **`<app>.enabled`** — whether an app is on. Default off.
3. **`[apps]`** — the order among what is on.
4. **`effective_tab_order()` = `["work"]` + enabled apps, ordered by (3).**

## Keep everything else

The core split, `AppTabId`'s schema, the builder gating, the live-reload rebuild,
and the unknown-tab message.

## Validation

`nix develop --command cargo nextest run -p thegn-core -p thegn-host` — whole
crates. **The sandbox mounts `/nix/store` read-only, so this usually fails
outright** — say so and stop if it does; the supervisor verifies centrally.

Never report a verdict for code you could not compile.
