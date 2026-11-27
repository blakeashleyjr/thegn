---
id: agent-integrations
title: What each coding agent gets
order: 7
parent: workflows
---

# What each coding agent gets

thegn hosts any terminal coding agent, but the agents it knows by name get
more: a launch form for `thegn agent run` and pipelines, skills that teach the
agent thegn's workflow, an MCP connection to thegn's own help and state, and
their login carried into remote environments. This page lists what each agent
gets and the one command that finishes the setup.

## The one command

```sh
thegn mcp wire
```

writes two entries into the MCP settings of every agent you configured in
`[[agents]]` and every supported agent installed for you:

- `thegn-serve` runs `thegn mcp serve`: the agent can search thegn's help
  pages, read your configuration with secrets masked, and list sessions,
  worktrees and the merge queue. Its scopes come from `[mcp.serve] scopes`
  (read-only when unset) each time the agent starts it.
- `thegn` runs `thegn mcp proxy`, the hub for the third-party servers you opt
  in under `[mcp_servers.<name>.proxy]`. See [[cli]].

Both entries hold a command and its arguments only; no secret is ever written
into an agent's settings. Each is marked as thegn's, running the command again
changes nothing, and `thegn mcp wire --remove` takes exactly those entries out
again. An entry with the same name that you wrote yourself is never changed or
removed, and a settings file thegn cannot parse is left alone. Wire one agent
with `--agent <kind>`.

The same command also writes lifecycle hooks, so the [[sidebar]] and attention
queue hear from the agent itself instead of guessing: Claude gets a `Stop` and
a `Notification` hook in `~/.claude/settings.json`, Codex a `notify` line in
`config.toml`. Each calls `thegn agent hook`, which tells the daemon a turn
finished or the agent needs you, and prints nothing. A hook or `notify` you
wrote yourself is never replaced; Codex runs only one notifier, so thegn skips
it and says so. The welcome tour offers all of this with one key (`w`) on its
agents step and writes nothing until you press it. If you finished the tour before agents were
wired, thegn asks once at a later start; either answer is remembered and the
answer is never asked again.
Remote Sprites and Vercel environments get the same MCP entries and hooks, run
through a resolver that finds `thegn` inside the environment.

Hermes keeps its servers in `~/.hermes/config.yaml` (or `$HERMES_HOME`) under
`mcp_servers:`. thegn edits that file as text, so your comments, key order and
quoting stay byte for byte; each thegn entry has a `# x-thegn-managed` comment
above it. A `mcp_servers` written as an inline `{}` mapping or a list is left
alone, and `wire` tells you why. These paths follow Hermes's documented layout
and have not been checked against every release.

Codex keeps its servers in `~/.codex/config.toml` (or `$CODEX_HOME`), which
thegn edits in place: your comments and tables stay as they were, and thegn's
tables carry a `# x-thegn-managed` comment.

## Per agent

| Agent                   | Launch and pipelines | Skills seeded     | `thegn mcp wire`        | Remote install |
| ----------------------- | -------------------- | ----------------- | ----------------------- | -------------- |
| Claude Code             | yes                  | `.claude/skills/` | `~/.claude.json`        | yes            |
| Codex                   | yes                  | `.agents/skills/` | `~/.codex/config.toml`  | yes            |
| pi                      | yes                  | `.pi/skills/`     | pi has no MCP client    | yes            |
| Hermes                  | interactive only     | remote only       | `~/.hermes/config.yaml` | yes            |
| Gemini CLI, Cursor, Amp | interactive only     | no                | yes                     | no             |
| Windsurf, VS Code, Zed  | not a terminal agent | no                | yes                     | no             |
| Aider                   | yes                  | no                | no                      | no             |

Every agent, named or not, gets the same activity tracking in the [[sidebar]]
(working, waiting, finished), the automatic build route for a project with
`route = "auto"`, and the model proxy when its `[[agents]]` entry sets
`route_via_proxy`. An agent that prints the terminal notification sequences
(`OSC 9` or `OSC 777`) raises a hand in the sidebar the moment it needs you.

## The skills

When `[skills] enabled = true`, each new worktree gets the shipped skills in
its agents' native folders. The `thegn` skill is the everyday guide: how to
see where the agent is (`thegn info`, `thegn env show`), how builds and tests
reach the project's runner (`thegn job route`), how to land or queue the
branch, how to ask you for attention (`thegn notify push`), how to start a
helper agent (`thegn agent run`), and what the sandbox will refuse. See
[[skills]] for the full list and how to add your own.

## Remote environments

A Sprites or Vercel environment installs Claude Code, Codex, pi and Hermes and
carries their logins and settings from this machine, so an agent there starts
signed in. Once the agents are installed and synced, thegn wires the same two
MCP entries inside the environment and seeds the `thegn` skill into each
agent's own skill folder (`[skills] enabled = true`). The entries do not use
this machine's `thegn` path: they run `thegn` from the environment's `PATH`,
or the `thegn` that the connection pushes there, and report plainly when
neither exists. Re-provisioning or restoring a snapshot changes nothing that is
already in place, and an entry or skill you wrote yourself is never replaced.
See [[environments]].

## Ordered backends

An agent role can document ordered alternatives under `[[agents.backends]]`.
Each entry describes a harness, optional model and harness-specific environment
overlay; `account` can bind it to a usage key or credential-home path. The
`agent list` command shows names and non-secret launch metadata, while
`agent backends` reports current usage headroom and reset time. `agent run`
selects the first backend with known headroom; `--backend <name>` pins one.
Pipeline stages continue to use their configured single backend.
