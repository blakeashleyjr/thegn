# Add a bounded argv-template tokenizer

Linear: THE-2103

## Why

Five planned consumers (WS02 sandbox backends, WS08 providers, WS09 connectors,
WS10 plugin bridges, WS11 agent presets) each turn a config-defined argv element
into exactly one argument. Five private grammars would drift apart, and a shell
lexer would leave quoting, expansion and malformed input ambiguous at a provider
or self-bridge boundary — the argument-injection class this contract closes.

The pure tokenizer already exists in `thegn-core` as the WS00-P1-07 stub. This
change records its contract, including the per-element size bound the stub
lacked, so config input and token expansion have an explicit ceiling before any
consumer adopts it.

## What changes

- `thegn_core::argv_template` accepts one caller-delimited UTF-8 argv element,
  resolves `{{`/`}}` to literal braces and `{a.b}` to a token path, and renders
  exactly one string.
- A token value is inserted verbatim: it is never split on whitespace and never
  re-parsed, so a value cannot become extra argv elements or forge a token.
- Shell quote, escape, `$`, backtick, whitespace and NUL characters are literal
  content; the grammar has no shell semantics and never executes a template.
- Template input and rendered output are each capped at
  `MAX_TEMPLATE_ELEMENT_BYTES` (16 KiB) with `TemplateError::TooLarge`.
- No consumer is added here. WS02-P4-02 owns the structural constructs (`each`,
  `when`, `splat`, `join`, filters), list tokens and per-verb token sets.

## Impact

- Roadmap: BB 763 (sandbox + remote programme), programme item WS00-P1-07.
- No config key, action, keybind, help page, capability, provider or CLI verb
  changes, so no help, config or surface ratchet moves.
- No SQLite schema change and no migration.
