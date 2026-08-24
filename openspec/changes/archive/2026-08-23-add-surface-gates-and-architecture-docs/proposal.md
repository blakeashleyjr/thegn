## Why

After the seam foundation, the architecture gates and the forge unification, the audit's remaining findings were on the _exposure surfaces_: adding an action needed 6–7 hand edits with only two tested couplings and three unit actions had no spec; the command palette dispatched four verbs by string, outside the keymap registry; notification kinds were listed by hand in four places; only 27 of 48 env knobs were exercised and nothing noticed a key without one; a typo'd config key was silently dropped; `nix/hm-module.nix` was an untested third copy of the schema (and rendered a dead `[dashboard]` section into every user's config); `--json` output was split between `emit_json` and hand-rolled printing. And the architecture itself lived in two drifting hand copies (CLAUDE.md, `openspec/config.yaml`) with no "how to add X" recipes and no specs for config or help.

## What Changes

- **Action registry complete**: `ConnectRoot`, `CloneOpen`, `NewEnvironment`, `SetupWizard` become `Action`s (the palette's string-keyed back doors are gone; their blocks moved into the action match); `enter-replay`, `toggle-recorder`, `paste-register` get `ActionSpec`s; `every_action_key_has_a_spec_and_round_trips` and `every_palette_key_is_an_action` pin both.
- **Notification kinds single-sourced**: `thegn notify push --kind` help is generated from `NotificationKind::ALL` (+ default priorities); `config.toml.example`'s prose lists all 25 and is tested against the enum.
- **Env overrides gated**: `tests/env_overlay_coverage.rs` — every shallow key has a `THEGN_*` knob or is pinned in `test/env-overlay-ratchet.txt` (shrink-only); every knob is exercised by `env_overlay_covers_every_knob` (extended from 27 to 48).
- **Unknown keys**: `thegn config validate --strict` reports `section.key: unknown key (did you mean …)`; lenient load still warns-and-drops.
- **home-manager drift**: `tests/hm_module_drift.rs` — rendered keys exist in the schema; enum options are subsets of `config_enum!` spellings. Removed the dead `dashboardIntervalSecs` option.
- **`--json` discipline**: `test/json-emit-ratchet.txt` pins the 13 files printing JSON outside `cmd::emit_json`.
- **Docs**: `docs/ARCHITECTURE.md` (single source, each section naming its gate), `docs/extending/` (8 recipes, each ending with the gate), CLAUDE.md Architecture/Source-map and `openspec/config.yaml` context reduced to the hard invariants + pointers; `docs/help/configuration.md` gains env vars, unknown keys, home-manager.
- **Specs**: new `config` and `help`; deltas on `keybindings` (registry completeness), `command-palette` (spec-driven rows), `notifications` (single-sourced kinds), `cli` (`--json` via `emit_json`).

## Capabilities

### New Capabilities

- `config`: layered loading, every key documented, env-override naming and completeness, unknown-key handling, strict validation, the schema as the contract the home-manager module derives from.
- `help`: the help corpus as a registry — page ⇔ `SOURCES`, action/zone/context coverage ratchets, generated pages never hand-written.

### Modified Capabilities

- `keybindings`: every action id has an `ActionSpec`; `key`/`from_key` round-trip.
- `command-palette`: every row is an action or a user `[[actions]]` entry.
- `notifications`: the kind list is the enum; CLI help and example prose derive from it.
- `cli`: machine-readable output goes through one emitter.

## Impact

- `crates/thegn-host/src/{keymap,keymap_specs,palette,run}.rs`, `cmd/notify.rs`; `crates/thegn-core/src/{notification,config_validate,config_tests}.rs`, `tests/{common/mod,config_example,env_overlay_coverage,hm_module_drift}.rs`; `nix/hm-module.nix`; `config/config.toml.example`; `test/{env-overlay,json-emit}-ratchet.txt`; `justfile`; docs as above.
- No schema/DB change; no render-path change (the moved palette blocks run in the same loop context as before).
- Roadmap: A.6, B/N (keymap), the config/help rows; audit plan row 4.
