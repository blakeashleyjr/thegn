# Coding-agent integrations out of the box

Linear: THE-2714

## Why

Claude Code, Codex, pi, Hermes and the other agents thegn hosts should use
thegn's features from their first launch. An audit of what thegn wires today
found the plumbing mostly present but unevenly reachable: Codex could not be
wired to thegn's MCP endpoint at all ("add this block by hand"), the wired
entry pointed only at the third-party proxy (which exposes nothing until the
user opts upstreams in), no seeded skill taught an agent the everyday thegn
workflow, and nothing documented what each agent actually gets.

## Audit matrix (2026-11-25, before this change)

Legend: **yes** wired automatically · **cmd** one explicit command · **cfg**
needs config · **no** missing · **n/a** not applicable.

| Feature                                  | Claude                  | Codex           | pi                 | Hermes                     | Gemini / Cursor / Amp / Windsurf / VS Code / Zed | OpenCode / Goose / Aider / Copilot / Grok |
| ---------------------------------------- | ----------------------- | --------------- | ------------------ | -------------------------- | ------------------------------------------------ | ----------------------------------------- |
| Harness adapter (launch/headless/resume) | yes                     | yes             | yes                | no (not in registry)       | no                                               | aider yes; rest no                        |
| MCP: `thegn mcp wire` adapter            | cmd                     | **no** (manual) | no (pi has no MCP) | no (YAML config)           | cmd                                              | no                                        |
| MCP: thegn's own `mcp serve` wired       | **no**                  | **no**          | n/a                | no                         | **no**                                           | no                                        |
| Seeded skills (mq/pipeline/supervise)    | yes                     | yes             | yes                | no (no project skill root) | no                                               | no                                        |
| Everyday "how to use thegn" skill        | **no**                  | **no**          | **no**             | no                         | no                                               | no                                        |
| Activity dot (CPU + unsolicited output)  | yes                     | yes             | yes                | yes                        | yes                                              | yes                                       |
| Authoritative waiting signal (OSC 9/777) | if agent emits          | if agent emits  | if agent emits     | if agent emits             | if agent emits                                   | if agent emits                            |
| Lifecycle hooks (Stop/Notification)      | no                      | no              | no                 | no                         | no                                               | no                                        |
| Model proxy routing + attribution        | cfg (`route_via_proxy`) | cfg             | cfg                | cfg                        | cfg                                              | cfg                                       |
| Credential carry to remote envs          | yes                     | yes             | yes                | yes                        | best-effort dotfile guess                        | best-effort dotfile guess                 |
| Job shim (`route = "auto"`)              | yes (host pane)         | yes (host pane) | yes (host pane)    | yes (host pane)            | yes (host pane)                                  | yes (host pane)                           |
| Installed in Sprites/Vercel              | yes                     | yes             | yes                | yes                        | no (left to `setup`)                             | no                                        |
| First-run detection (welcome tour)       | yes                     | yes             | yes                | no                         | no                                               | aider yes                                 |
| Usage / sessions                         | yes                     | yes             | no                 | no                         | antigravity usage only                           | no                                        |

## What Changes

- **New embedded `thegn` skill** (gate `always`, Claude/Codex/pi, seeded on
  create, startup and explicit seeding): teaches `thegn info`/`env show`, the
  automatic build route and `thegn job run`, `land` and the merge queue,
  `thegn notify push` for attention, `thegn agent run` helpers, the sandbox's
  limits, and the MCP server. Every fenced command is parsed against the live
  CLI by the existing skill test.
- **`thegn mcp wire` writes two entries**: `thegn` (the proxy, unchanged) and
  `thegn-serve` (`thegn mcp serve`, scopes from config, `read` by default), so
  a wired agent can search thegn's help and read live state with no upstream
  configuration. Both are argv-only, marker-tagged and removable.
- **Codex adapter**: `thegn mcp wire --agent codex` edits
  `~/.codex/config.toml` (or `$CODEX_HOME/config.toml`) in place with
  `toml_edit`, marking each `[mcp_servers.<name>]` table with a comment so user
  comments and tables survive and a user-authored `thegn` table is never
  touched.
- **Default targets include installed agents**: a bare `thegn mcp wire` wires
  configured `[[agents]]` plus every adapter whose agent home exists, so a new
  user without `[[agents]]` gets a result instead of a refusal. Wiring stays an
  explicit command (the mcp-proxy spec forbids launch side effects).
- **Docs**: a new help page, `agent-integrations`, lists what each agent gets.

## Out of scope (follow-ups)

- Lifecycle hooks (Claude `Stop`/`Notification`, Codex `notify`) that report
  working/waiting/done authoritatively; needs a hook-to-pane channel because
  hook stdout does not reach the PTY scanner.
- Hermes as a registry harness (headless form, skill root) and a YAML adapter
  for `~/.hermes/config.yaml` `mcp_servers`.
- Remote envs: the wired command is the host's thegn path; a sandbox needs the
  in-sandbox thegn path (or the bridge) before the carried entry works there.
- Offering `thegn mcp wire` from the welcome tour's agent step.

## Impact

- `crates/thegn-core/src/mcp/wire.rs`, `crates/thegn-host/src/cmd/mcp_proxy_cmd.rs`,
  `crates/thegn-core/src/skills.rs`, `extensions/skills/thegn/SKILL.md`.
- Specs: `mcp-proxy` (wire requirement), `skills` (registry contents).
