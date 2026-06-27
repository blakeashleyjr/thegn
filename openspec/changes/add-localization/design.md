# Design

## Mechanism (superzej-core `i18n.rs`)

- **Embedded assets:** `locales/<lang>/main.ftl` baked into the binary via
  `fluent-templates` + `rust-embed` (zero runtime file I/O).
- **Global resolver:** a thread-safe static (`ArcSwap`/`OnceLock`) holds the active
  `LanguageIdentifier`; a `t!("key")` / `t!("key", arg=value)` macro evaluates
  against it.
- **Detection:** `sys-locale` reads the host locale; `[ui] language` (default
  `"auto"`) overrides. Resolved **once** in the `szhost::startup` waterfall, before
  first render.

## Layout invariant (cells, not bytes)

Translations change geometry (a 4-cell label may become 9). Rule: every translated
string passes through `unicode-width` (`t!("key").width()`), layouts use
responsive/truncating measurement, and padding is applied **after** translation
(`format!("{:^10}", t!("key"))`). Crate additions go in `superzej-core` so
`superzej-host` is insulated.

## Invariants

Zero runtime I/O (compiled-in), resolves before first frame (sub-300ms preserved),
no idle cost. AI-additive: this localizes only superzej's own chrome, never user
data or the agent protocol.
