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

- [x] 4.1 Lifecycle hooks for Claude (`Stop`, `Notification` in
      `settings.json`) and Codex (`notify`), written by the same marked
      writer as `thegn mcp wire` and reporting through the hidden
      `thegn agent hook` verb over the control socket (finished turn and
      needs-you; working stays CPU-derived). Remote sandboxes get the same
      hooks, with the in-sandbox resolver as the hook command.
- [x] 4.2 Hermes registry harness and YAML MCP adapter (`~/.hermes/config.yaml`,
      `HERMES_HOME`; comment-marked, text-edited so user text is untouched).
- [x] 4.3 Remote envs: after the agent install and login sync, wire MCP with an
      in-sandbox resolver command and seed the `thegn` skill, idempotently.
- [x] 4.4 Offer the wiring from the welcome tour's agent step as an explicit
      one-key accept (`w`), written at finish.
- [x] 4.5 Show the same offer once to an existing user whose detected agents
      are not wired (outside the first-run tour).
