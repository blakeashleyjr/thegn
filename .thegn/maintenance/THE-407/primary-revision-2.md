# Primary review — THE-407 revision 2 (reviewing row 676)

You implemented the split correctly at the core layer — `app_tab_config_*` both
pass now, so `BUILTIN_TABS` (known) and the default order (`["work"]`) are properly
separated. **My ruling was under-specified one layer down**, and your own new tests
are what exposed it.

## What the failures say

```
FAIL apps::registry::tests::enabled_builders_appear_in_the_host_tab_order
FAIL apps::tests::observe_tab_registered_only_when_enabled
FAIL apps::tests::reload_can_enable_a_lazy_app_and_select_it_as_the_new_default
FAIL config_admission::tests::cli_schema_is_checked_before_lenient_override_deserialization
```

`enabled_builders_appear_in_the_host_tab_order` opens with
`assert_eq!(enabled(&cfg).count(), 0, "apps are opt-in")` on a **default** config,
then flips `cfg.observe.enabled = true` and expects `["observe"]`.

Meanwhile `registry.rs:62` enumerates from `AppsConfig::BUILTIN_TABS` — the
**known** list. So a default config produces a registered `observe` builder, and
"apps are opt-in" fails.

## DECISION — there are THREE concepts, not two

I named two last round. Your tests establish a third, and it is the one that gates
registration:

1. **`BUILTIN_TABS`** — the ids that _exist_. Used to validate `[apps]` and to list
   known tabs in an error. Includes `observe`.
2. **`<app>.enabled`** (e.g. `cfg.observe.enabled`) — whether an app is switched on
   at all. **This is what gates registration.** Default false: apps are opt-in.
3. **`[apps].effective_tab_order()`** — the order the user wants among apps that
   are enabled. Defaults to `["work"]`.

So: **the registry enumerates candidates from (1) but registers only those passing
(2), and the tab order is (3) restricted to what (2) admitted.** `registry.rs:62`
currently stops at (1), which is the whole failure.

An app named in `[apps]` but not enabled is **not** a validation error — it is
simply absent from the order. Naming an id that does not exist in (1) _is_ an
error, with the message listing the known ids. Those two cases must not collapse
into one.

`config_admission::cli_schema_is_checked_before_lenient_override_deserialization`
is the same root cause reaching the schema, as I guessed last round — recheck it
after the gating change rather than treating it separately.

## Why I keep having to sharpen this

This lane has now needed three rulings because the original code used one list for
what turned out to be three questions: _does this exist_, _is it on_, and _where
does it sit_. Each round peeled off one. **Put a comment at `BUILTIN_TABS` naming
all three and which function answers which** — that is the artifact that stops a
fourth round, and it is worth more than the code change.

## Keep as implemented

The core-level split (both `app_tab_config_*` tests now pass — do not disturb
them), `AppTabId`'s schema with schemars 0.8's `is_referenceable() -> false`, the
live-reload `AppHost` rebuild, and the unknown-tab message naming the known ids.

## Validation

`nix develop --command cargo nextest run -p thegn-core -p thegn-host` — the **whole**
crates. **The pipeline sandbox mounts `/nix/store` read-only, so this usually fails
outright** — say exactly that and stop if it does; a supervisor runs the crate-wide
verification centrally and will report the result.

Never report a verdict for code you could not compile; state what you could not run.
