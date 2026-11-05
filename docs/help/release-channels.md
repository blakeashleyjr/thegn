---
id: release-channels
title: Release channels
order: 14
---

# Release channels

thegn ships as one binary that runs in one of two **channels**. The
channel decides which subsystems are live — not which code was compiled,
so nothing is missing, it is simply inert and hidden.

- **stable** — the regular alpha shell. The default.
- **dev** — everything, including the experimental subsystems below.

`thegn doctor` prints the channel you are running in and the state of
every gated feature. Set `THEGN_CHANNEL=dev` to switch for one run.

## What is dev-only

If you read about one of these elsewhere in this help and cannot find it,
this is why:

| Feature     | Covers                                                                     | Config                                          |
| ----------- | -------------------------------------------------------------------------- | ----------------------------------------------- |
| `remote`    | worktrees on another machine over SSH/mosh                                 | `[sandbox.remote]`, `[env.*] placement = "ssh"` |
| `providers` | cloud execution (Fly / DO / VPS / Machine0 / Daytona) and the managed pool | `[host.*]`, `[env.*]` provider envs             |
| `placement` | the multi-host placement engine                                            | `[placement]`                                   |
| `voice`     | command-backed voice mode                                                  | `[voice]`                                       |

On the stable channel, SSH and provider `[env.*]` environments are removed
when the config loads. If a worktree's `[sandbox] default_env` or selection
names one, its pane halts by default with a hint to set `THEGN_CHANNEL=dev`;
`failover = "ask"` prompts instead. With `failover = "auto"`, the pane falls
back to the host.

Issue trackers including Linear, Jira, Kaneo, and GitHub are available in both
channels. GitHub PR viewing is also stable. See [[review-a-pr]].

## Stable features that start off or are easy to miss

These two are part of the stable channel; they are listed here because they
look like the dev-only features above.

| Feature       | Covers                                                        | Config                                                |
| ------------- | ------------------------------------------------------------- | ----------------------------------------------------- |
| `observe`     | the Observe dashboard and fleet-view app tab (off by default) | `[observe] enabled = true`; switch with `Alt-2`       |
| `plugin_apps` | resident plugin app tabs                                      | `[apps]` (`default_tab`, `tab_order`), plugin app ids |

Plugin processes are native and unsandboxed; see [[plugins]] and [[sandboxing]].

## What stable guarantees

Everything else. The AI-free terminal shell — git, panes, the [[sidebar]], the
[[panel]], [[daemon-and-sessions]], the merge queue and `land`/`integrate`, the
[[sandboxing]] backends, issue trackers, Observe and plugin app tabs, and every
[[cli]] verb except `thegn host` and `thegn placement` — is stable and is never
channel-gated. Those two verbs refuse on the stable channel with a pointer to
the dev channel.

## How the gate behaves

Enforcement happens at the edges, never by compiling code out:

- Config toggles for a disallowed feature are **neutralised at load**, so
  a stray `[placement]` or `[voice]` block in a stable run is inert rather
  than an error.
- Dev-only verbs and UI surfaces are hidden. Tracker commands remain available
  in both channels when an issue provider is configured.

That means switching channels needs no reinstall and no config
migration — the same config file behaves correctly in both.

See [[configuration]] for the config layers these keys live in.
