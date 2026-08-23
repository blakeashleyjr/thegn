## 1. Implementation

- [x] 1.1 `host_contract()` accepts PaletteAction + grants `surface:palette`
- [x] 1.2 `handlers::plugins::{palette_items, invoke_palette_action}` + `PluginsHost::run_one_shot`
- [x] 1.3 run.rs: items extended at the OpenPalette site; `plugin:` dispatch arm before Action lookup
- [x] 1.4 Tests (list/route/disabled/unknown); docs (help + extending pages)

## 2. Gate

- [x] 2.1 clippy + host/svc suites + `just lint`; openspec validate
