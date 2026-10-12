# Tasks

## 1. i18n core (thegn-core)

- [ ] 1.1 `i18n.rs`: embed `locales/<lang>/main.ftl` (`fluent-templates` +
      `rust-embed`); global resolver + `t!` macro — **unit tests** (key lookup,
      interpolation, pluralization, missing-key fallback).
- [ ] 1.2 `UiConfig` + `[ui] language` (default `auto`) parsing; `sys-locale`
      detection — **unit tests** (auto vs explicit).
- [ ] 1.3 Resolve locale once in the `thegn::startup` waterfall before first render.

## 2. Layout safety (host)

- [ ] 2.1 Route chrome strings through `t!`; ensure `unicode-width` measurement +
      truncation in `chrome.rs`/`sidebar.rs` (no hardcoded byte-length padding) —
      **test** a long translation truncates within the panel budget.

## 3. Validate

- [ ] 3.1 Run `just ci` (includes `openspec-validate`).
