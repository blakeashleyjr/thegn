# Tasks — resident plugin app tabs (THE-1673)

## 1. OpenSpec + delivery + channel gate + `[apps]` ids

- [x] 1.1 Change folder — `proposal.md`, `design.md`, `tasks.md`, and both spec deltas.
- [x] 1.2 Delivery index/issues — `delivery/index.json` and `delivery/issues.json`.
- [x] 1.3 `Feature::PluginApps` + clamp + host bit + clamp notice + release-channels help row — `channel.rs`, `config_resolve.rs`, `channel_state.rs`, and `release-channels.md`.
- [x] 1.4 `[apps]` plugin ids (schema, validation, example) — `AppsConfig::is_plugin_tab_id` and `app_tabs_are_schema_listed_and_unknown_ids_are_actionable`.

## 2. Core contract 0.4

- [x] 2.1 `plugin_app` module — `crates/thegn-core/src/plugin_app.rs` and `tests/plugin_app.rs`.
- [x] 2.2 `AppTab` support row + `surface:app` — `app_tabs_require_resident_mode_background_permission_and_surface`.
- [x] 2.3 `PluginSpec.background` + `background:sync` grant — `app_tabs_require_resident_mode_background_permission_and_surface`.
- [x] 2.4 `authorize_app_update` — `authorize_app_update_audits_app_capability_and_checks_surface_quota`.
- [ ] 2.5 Schema snapshot 0.4 regenerated.
- [x] 2.6 Docs version markers — `docs/extending/plugin.md` and `docs/help/plugins.md`.

## 3. thegn-plugin-ui crate + tg-kit

- [x] 3.1 Crate + workspace — `crates/thegn-plugin-ui/src/lib.rs` defines `UiEngine`.
- [x] 3.2 tg-kit `AppTile::id -> &str` and `command` — `crates/tg-kit/src/tile.rs::tests::command_defaults_to_unhandled`.
- [x] 3.3 F2 recovery as a public method — `crates/thegn-plugin-ui/src/tests.rs::f2_is_not_interpreted_by_the_engine`.

## 4. Transport on main's runtime

- [ ] 4.1 Shared request-id allocator + session token.
- [ ] 4.2 Egress control reservation + replace coalescing + active-write accounting.
- [ ] 4.3 ProviderBridge on the shared allocator.
- [ ] 4.4 Inbox focus/request barriers + byte bounds.

## 5. App tile + app host

- [ ] 5.1 `apps/plugin.rs`.
- [ ] 5.2 `apps/mod.rs` dynamic ids + `sync_plugins` + `theme_told`.

## 6. Plugin handler routing

- [ ] 6.1 Entry apps + grants + activate params.
- [ ] 6.2 App update routing + focus tickets.
- [ ] 6.3 Response routing + SessionGen exit/respawn.
- [ ] 6.4 Experimental gate at build.

## 7. Keymap + run loop + help + idle proof

- [ ] 7.1 Actions `plugin-app-drafts` / `plugin-app-requests`.
- [ ] 7.2 `handlers/plugin_apps.rs` glue + run.rs hooks.
- [ ] 7.3 Help pages + CHANGELOG + ARCHITECTURE.
- [ ] 7.4 Idle test.
- [ ] 7.5 Final gates (`just ci` once, primary).
