---
id: release-channels
title: Release channels
order: 14
---

# Release channels

thegn ships as one binary that runs in one of two **channels**. The
channel decides which subsystems are live — not which code was compiled,
so nothing is missing, it is simply inert and hidden.

- **stable** — the regular pre-alpha shell. The default.
- **dev** — everything, including the experimental subsystems below.

`thegn doctor` prints the channel you are running in and the state of
every gated feature. Set `THEGN_CHANNEL=dev` to switch for one run.

## What is dev-only

If you read about one of these elsewhere in this help and cannot find it,
this is why:

| Feature     | Covers                                                                     | Config             |
| ----------- | -------------------------------------------------------------------------- | ------------------ |
| `remote`    | worktrees on another machine over SSH/mosh                                 | `[sandbox.remote]` |
| `providers` | cloud execution (Fly / DO / VPS / Machine0 / Daytona) and the managed pool | `[host.*]`         |
| `observe`   | the Observe dashboards and fleet-view tab                                  | `[observe]`        |
| `placement` | the multi-host placement engine                                            | `[placement]`      |
| `trackers`  | Linear / Jira / Kaneo issue trackers                                       | `[issues]`         |

GitHub PR and issue **viewing** is stable and is not gated — only the
multi-tier trackers are. See [[review-a-pr]].

## What stable guarantees

Everything else. The AI-free terminal shell — git, panes, the
[[sidebar]], the [[panel]], [[daemon-and-sessions]], the merge queue and
`land`/`integrate`, the [[sandboxing]] backends, the whole [[cli]] — is
unconditionally stable and never channel-gated.

## How the gate behaves

Enforcement happens at the edges, never by compiling code out:

- Config toggles for a disallowed feature are **neutralised at load**, so
  a stray `[observe]` block in a stable run is inert rather than an error.
- The matching UI and CLI surfaces are hidden.

That means switching channels needs no reinstall and no config
migration — the same config file behaves correctly in both.

See [[configuration]] for the config layers these keys live in.
