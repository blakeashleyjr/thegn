# Add resident plugin app tabs

## Why

Resident plugins can currently fill a statusbar segment, provide palette actions,
send notifications, or implement an issue provider. A full workspace app such as
a task manager or dashboard needs a tab, an input contract, and a bounded widget
protocol. The `tg/sturdy-pearl` proof of concept built this on a base that main
has since rebuilt; this change re-lands only the infrastructure on main's
resident runtime.

## What Changes

- Add the additive Plugin API 0.4.0 `AppTab` extension point and the
  `thegn_core::plugin_app` AppView contract.
- Add the `background` spec permission for `background:sync`.
- Let `[apps]` accept plugin tab ids.
- Add a new `thegn-plugin-ui` crate.
- Add a host adapter on main's SessionGen, inbox, and respawn mechanisms.
- Add the `plugin-app-drafts` and `plugin-app-requests` keymap actions.
- Gate app tabs behind the dev-channel `plugin_apps` feature.

## Impact

- Crates: thegn-core, thegn-svc, thegn-host, tg-kit, and the new
  thegn-plugin-ui.
- Docs: `docs/api/plugin-api-0.4.json`, docs/help/plugins.md,
  docs/help/release-channels.md, and docs/extending/plugin.md.
- Roadmap: tasks.md items 198, 199, and 206. Linear: THE-1673, parent
  THE-1672.
- Related: THE-107 and `decide-plugin-ui-extension-surfaces` (adopted key zones
  are limited to a plugin's focused surfaces; an app tab is a plugin's focused
  surface), THE-165, THE-161, THE-536, and THE-537.

## Out of Scope

- Preview CLI (THE-1674).
- SDK (THE-1675).
- Todoist example (THE-1676).
- OS sandboxing (THE-929).
