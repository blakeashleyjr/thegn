## 1. Keymap / palette

- [x] 1.1 `Action::{ConnectRoot, CloneOpen, NewEnvironment, SetupWizard}` + key/from_key + specs; palette blocks moved into the action match
- [x] 1.2 Specs for `enter-replay`, `toggle-recorder`, `paste-register`; help pages claim + mention all seven ids
- [x] 1.3 `every_action_key_has_a_spec_and_round_trips`, `every_palette_key_is_an_action`

## 2. Notifications

- [x] 2.1 `Priority::as_str`; `thegn notify push --kind` long help from `NotificationKind::ALL`
- [x] 2.2 Example prose lists all 25 kinds; `example_config_prose_names_every_kind`

## 3. Config gates

- [x] 3.1 `tests/common/mod.rs` schema walker shared; `config_example.rs` uses it
- [x] 3.2 `tests/env_overlay_coverage.rs` + `test/env-overlay-ratchet.txt` (359 seeded); `env_overlay_covers_every_knob` extended to all 48 knobs
- [x] 3.3 Unknown-key strict validation with did-you-mean; `LEGACY_KEYS`; tests
- [x] 3.4 `tests/hm_module_drift.rs`; dead `dashboardIntervalSecs` option removed
- [x] 3.5 `test/json-emit-ratchet.txt` (13 seeded) wired into `just lint` / `ratchet-update`

## 4. Docs

- [x] 4.1 `docs/ARCHITECTURE.md`; CLAUDE.md Architecture/Source map → invariants + pointer; `openspec/config.yaml` context → invariants + pointer; stale-docs guard exempts ban descriptions
- [x] 4.2 `docs/extending/` (README + 8 recipes)
- [x] 4.3 `docs/help/configuration.md`: env vars, unknown keys, home-manager; `README.md` pointer to ARCHITECTURE.md
- [x] 4.4 Specs: `config`, `help` (new); `keybindings`, `command-palette`, `notifications`, `cli` (deltas)

## 5. Gate

- [x] 5.1 clippy per crate, core/svc/host suites, `just lint`, coverage, doc-check, openspec validate, fmt (no e2e)
      _(gate run: clippy core/host; thegn-core 2446 + thegn-host 2025 tests; just lint; coverage ≥95%; doc-check; openspec validate; treefmt. e2e skipped by policy.)_
