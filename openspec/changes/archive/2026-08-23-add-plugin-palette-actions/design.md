## Context

Palette rows were pinned to two classes (Actions by `ACTION_SPECS` id, user `[[actions]]`) precisely to kill string-keyed back doors. Plugin actions need a third class without reopening that hole.

## Decisions

- **Namespaced keys, dispatched first.** `plugin:<plugin>:<contribution>` is parsed by its own dispatch arm ahead of `Action::from_key`; the items are added at the palette _call site_ from loop-owned plugin state, so `build_command_palette_items` (and its pinning test) stays pure-config.
- **No chord binding yet.** The wire's `chord` hint stays declarative; binding plugin actions into the keymap means dynamic ACTION_SPECS and help-ratchet interaction — deferred until a real plugin needs it.
- **One-shot invocation = run the plugin once.** The action event is implicit in the run (a one-shot plugin has no stdin); residents get the explicit `on_event`.

## Risks / Trade-offs

- A plugin's palette label is attacker^Wauthor-controlled text; it is prefixed with the plugin id so a row can't impersonate a built-in verb.
