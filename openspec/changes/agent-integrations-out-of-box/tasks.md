# Tasks

## 1. Everyday skill

- [x] 1.1 Add `extensions/skills/thegn/SKILL.md` (gate `always`,
      `claude,codex,pi`, `create,startup,explicit`) and register it in the
      embedded manifest.
- [x] 1.2 Update the pinned registry name lists (core, completion, CLI tests)
      and the skills help page.

## 2. MCP wiring

- [x] 2.1 Generalize the pure JSON merge to named entries and add the
      `thegn-serve` entry beside the proxy entry.
- [x] 2.2 Add the pure TOML merge (comment-marked, argv-only, refuses foreign
      and non-table containers) with round-trip tests.
- [x] 2.3 Add the Codex adapter (honouring an absolute `CODEX_HOME`) and make a
      bare `wire` target installed agents as well as configured ones.
- [x] 2.4 Adapter tests: idempotent, reversible, user keys kept, foreign entry
      refused without writing, unparseable files refused.

## 3. Documentation

- [x] 3.1 Add the `agent-integrations` help page with the per-agent table.
- [x] 3.2 Update `docs/cli.md` for the two entries and the Codex adapter.

## 4. Follow-ups

- [ ] 4.1 Lifecycle hooks reporting working/waiting/done for Claude and Codex.
- [ ] 4.2 Hermes registry harness and YAML MCP adapter.
- [ ] 4.3 Remote envs: rewrite the carried MCP command to the in-sandbox thegn.
- [ ] 4.4 Offer `thegn mcp wire` from the welcome tour's agent step.
