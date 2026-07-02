# Tasks

## 1. Templater (superzej-core)

- [ ] 1.1 `compact.rs`: tokenizer (with char-class fallback), grouping, and
      `compact(lines, cfg) -> Compaction { groups: [{template, count, samples,
slots}] }` emitting in ascending first-occurrence order — **unit tests**:
      repeated lines collapse with correct count, distinct lines stay distinct,
      slot captures the varying token, compact-JSON falls back to char-class,
      emission order is stable across runs (determinism).
- [ ] 1.2 Size gate: `maybe_compact(window, cfg)` returns raw below the configured
      threshold, compacted above — **unit tests**: below threshold returns input
      unchanged, above threshold compacts, threshold boundary.

## 2. Context-path wiring (superzej-host / superzej-proxy)

- [ ] 2.1 When enabled by config, route the captured scrollback window through
      `maybe_compact` on the ACP/proxy context feed before it becomes agent
      context; with the switch off, pass raw scrollback (default off).

## 3. Docs + validate

- [ ] 3.1 Document the compaction switch, the size threshold, and the "loses on
      small windows" rationale in `config/config.toml.example` + the AI/context doc
      section.
- [ ] 3.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
