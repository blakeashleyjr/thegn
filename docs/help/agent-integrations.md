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

Codex keeps its servers in `~/.codex/config.toml` (or `$CODEX_HOME`), which
thegn edits in place: your comments and tables stay as they were, and thegn's
tables carry a `# x-thegn-managed` comment.

## Per agent

| Agent                   | Launch and pipelines | Skills seeded     | `thegn mcp wire`       | Remote install |
| ----------------------- | -------------------- | ----------------- | ---------------------- | -------------- |
| Claude Code             | yes                  | `.claude/skills/` | `~/.claude.json`       | yes            |
| Codex                   | yes                  | `.agents/skills/` | `~/.codex/config.toml` | yes            |
| pi                      | yes                  | `.pi/skills/`     | pi has no MCP client   | yes            |
| Hermes                  | interactive only     | no                | not yet                | yes            |
| Gemini CLI, Cursor, Amp | interactive only     | no                | yes                    | no             |
| Windsurf, VS Code, Zed  | not a terminal agent | no                | yes                    | no             |
| Aider                   | yes                  | no                | no                     | no             |

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
signed in. The wired MCP entries travel with the settings but name this
machine's `thegn` path; inside a remote environment they work only where that
path exists. See [[environments]].
