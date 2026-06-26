# Zero-Cost Terminal Localization Strategy

Date: 2026-06-25

## Goal

Provide a robust, type-safe, and zero-runtime-I/O localization (i18n) layer for the `superzej` terminal IDE. The solution must respect the project's strict sub-300ms startup bounds, provide expressive pluralization, and be fully aware of the strict cell-width requirements of the terminal UI.

## Non-goals

- Exposing language packs dynamically via the filesystem without recompilation in Phase 1.
- Replacing the agent/LLM protocol translation layer (the proxy is separate from the UI localization).
- Auto-translating user data (git branch names, commits, or subprocess output).

## Architecture: Fluent + Embedded Templates

Mozilla's Fluent system (`.ftl` files) provides the needed expressiveness for pluralization, gender, and interpolation. To meet the zero-I/O invariant, translations are compiled directly into the binary using `fluent-templates` and `rust-embed`.

### Core Mechanism (`crates/superzej-core/src/i18n.rs`)

1. **Embedded Assets:** Locale files (e.g., `locales/en-US/main.ftl`) are baked into the binary.
2. **Global Resolver:** A thread-safe static (`ArcSwap` or `OnceLock`) holds the active `LanguageIdentifier`.
3. **Macro Interface:** A global `t!("key")` or `t!("key", arg=value)` macro evaluates the key against the active language.

### OS Detection & Configuration

1. **Fallback:** `sys-locale` reads the host system's locale.
2. **Override:** The `config.toml` gets a new `[ui]` table:
   ```toml
   [ui]
   language = "ja-JP"  # "auto" by default
   ```
3. **Initialization:** The locale is resolved exactly once during the `szhost::startup` waterfall, prior to the first render.

## The TUI Layout Invariant (Grid Cells vs Bytes)

The largest challenge with terminal i18n is that translations change the layout geometry. A button that says "Save" (4 cells) might translate to "Speichern" (9 cells).

**Rule 1:** Translations **must** be passed through `unicode-width` to calculate actual cell span. `t!("key").width()` is the required pattern.
**Rule 2:** Layouts (like `chrome.rs` and `sidebar.rs`) must use responsive/flex measurements or explicit truncation when string length exceeds the panel budget.
**Rule 3:** Translated strings should avoid hardcoded padding. If padding is needed for alignment, `termwiz::Surface` geometry or format padding (e.g., `format!("{:^10}", t!("key"))`) should be applied _after_ translation.

## Agent Layer Considerations (Track 2)

While the UI translates its own chrome, the proxy must communicate the user's active locale to the agents.

- The Model Context Protocol (MCP) tool descriptions sent to the proxy should inject the resolved locale.
- System prompts must instruct the agent to communicate in the language corresponding to `[ui].language`.

## File Structure

- `crates/superzej-core/src/i18n.rs`: The translation resolver and `t!` macro.
- `crates/superzej-core/locales/<lang>/main.ftl`: The fluent strings.
- `crates/superzej-core/src/config.rs`: Added `UiConfig` and `language` parsing.

## Summary of Crate Additions

- `fluent-templates` (and `fluent-bundle`, `unic-langid`) for compile-time `.ftl` embedding.
- `sys-locale` for fast, zero-dependency host OS language discovery.

All dependencies go into `superzej-core`, hiding the complexity from `superzej-host`.
