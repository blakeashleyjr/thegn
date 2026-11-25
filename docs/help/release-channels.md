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
To keep a dev instance beside your stable one, see [[two-instances]].

## What is dev-only

If you read about one of these elsewhere in this help and cannot find it,
this is why:

| Feature     | Covers                                                                                                                                                | Config                                                     |
| ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| `remote`    | worktrees on another machine over SSH/mosh, and Kubernetes placement                                                                                  | `[sandbox.remote]`, `[env.*] placement = "ssh"` or `"k8s"` |
| `providers` | cloud execution other than Sprites and Vercel (Fly / DO / Hetzner / Machine0 / Daytona / custom), the managed pool, `thegn fleet` for those providers | `[host.*]`, other `[env.*]` provider envs                  |
| `placement` | the multi-host placement engine                                                                                                                       | `[placement]`                                              |
| `voice`     | command-backed voice mode                                                                                                                             | `[voice]`                                                  |
| `lead`      | the Lead meta agent: a hosted agent CLI in a global tab                                                                                               | `[lead]`                                                   |

### Sandbox and remote programme features (not yet implemented)

These nine features are reserved for the sandbox and remote programme. They
are experimental, so they are dev-only, and each one reads "experimental, not
yet implemented" in `thegn doctor` until the work that owns it lands. No config
key turns them on yet; listing them now keeps the feature set stable while the
programme is built.

| Feature            | Covers                                                                   | Config              |
| ------------------ | ------------------------------------------------------------------------ | ------------------- |
| `sandbox_backends` | config-defined, shipped and recipe sandbox backends; live fork           | not yet implemented |
| `linux_confine`    | the Landlock + seccomp confinement helper on bwrap, systemd and Landlock | not yet implemented |
| `egress_proxy`     | `network = "allowlist"`, presets, approvals, brokering, external proxies | not yet implemented |
| `seatbelt`         | the macOS Seatbelt backend                                               | not yet implemented |
| `win_isolation`    | the Windows MXC isolation backend                                        | not yet implemented |
| `plugin_backends`  | sandbox backends supplied by plugins                                     | not yet implemented |
| `agent_presets`    | per-agent presets applied at launch                                      | not yet implemented |
| `private_copy`     | private-copy workspaces (`workspace.mode = "private-copy"`)              | not yet implemented |
| `escalation`       | in-pane escalation requests                                              | not yet implemented |

On the stable channel, SSH, Kubernetes and provider `[env.*]` environments
other than Sprites and Vercel are removed when the config loads. If a worktree's `[sandbox] default_env` or selection
names one, its pane halts by default with a hint to set `THEGN_CHANNEL=dev`;
`failover = "ask"` prompts instead. With `failover = "auto"`, the pane falls
back to the host.

Issue trackers including Linear, Jira, Kaneo, and GitHub are available in both
channels. GitHub PR viewing is also stable. See [[review-a-pr]].

## Sprites and Vercel are stable

Sprites environments (`provider = "sprites"`, see [[environments]]) and Vercel
Sandbox environments (`provider = "vercel"`, see [[vercel-sandbox]]) are
supported on the stable channel: interactive worktrees, `thegn env` verbs, and
`thegn job run` with a Sprites or Vercel runner. The panel's system →
environments section is shown on both channels. The stable clamp keeps exactly
these two provider kinds. In `thegn doctor` the `providers` feature still reads
dev-only, because it names what remains gated: every other provider, the
managed `[host.*]` pool and the `thegn host` and `thegn placement` verbs.
`thegn fleet list`, `cost` and `inspect` run on stable and show Sprites and
Vercel environments only. The **New environment…** wizard offers only the
kinds the channel allows (stable: `local`, `sprites`, `vercel`).

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
[[cli]] verb except `thegn host` and `thegn placement` — is stable and is
never channel-gated. Those two verbs refuse on the stable channel with a
pointer to the dev channel. Sprites and Vercel environments, `thegn fleet`
(for them) and `thegn job run` are stable (see above).

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

## Preparing a release

`thegn release prepare --bump <alpha|beta|rc|patch>` does the version bump for
a release in the current worktree, which must be clean. It rewrites four
files: the `[workspace.package]` version in `Cargo.toml`, every workspace entry
in `Cargo.lock`, the status line in `README.md`, and `CHANGELOG.md` (the
`[Unreleased]` body becomes the new dated release section, a fresh empty
`[Unreleased]` goes on top, and the compare links are updated). It does not
commit, tag or push; review the diff and commit it yourself.

- `alpha` takes `-alpha.N` to `-alpha.N+1`. `beta` and `rc` move to the next
  channel (`-beta.1`, `-rc.1`) or count up within it, and refuse to go
  backwards. `patch` turns a pre-release into its stable version, or a stable
  version into the next patch.
- `--from <ref>` (default `HEAD`) and `--branch <name>` (default
  `tg/release-<new version>`) pick where the work happens; a branch other than
  the current one is created from `--from` with `git switch -c`.
- `--date YYYY-MM-DD` overrides today's date in the changelog heading.
- `--lock-from <path>` takes `Cargo.lock` from another worktree (a directory or
  a lock file) before bumping.
- `--dry-run` shows the files that would change and writes nothing (it also
  works with `--from <ref>`: the files are read from that ref). `--json`
  prints the result as one JSON object.
- Every rewrite is computed before anything changes, so a refusal (already
  bumped, a missing marker) leaves your branch and files untouched.

Running it twice refuses: the changelog already has the new version's heading.
