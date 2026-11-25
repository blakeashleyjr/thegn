## ADDED Requirements

### Requirement: Sprites and Vercel environments are available in the stable channel

In the `stable` channel the config clamp SHALL keep every `[env.<name>]` whose
`provider` is `sprites` or `vercel` (case-insensitive), so worktrees can be
placed on them and `[jobs] runner` can name them. It SHALL still remove every
other provider env (any other or empty `provider` with `placement =
"provider"`, or a non-empty `provider` naming another kind), every Kubernetes
env, and every `[host.*]` entry (SSH envs are covered by
`ship-ssh-clientserver-on-stable`). `thegn job` SHALL NOT be refused in
the stable channel; `thegn host`, `thegn fleet` and `thegn placement` SHALL
remain refused. The `providers` feature SHALL remain experimental and name the
dev-only remainder in `thegn doctor`.

#### Scenario: Stable keeps Sprites and Vercel envs

- **WHEN** a stable binary loads `[env.spr]` with `provider = "sprites"`,
  `[env.vsb]` with `provider = "vercel"` and `[env.fly]` with
  `provider = "fly"`, all `placement = "provider"`
- **THEN** `spr` and `vsb` remain configured, `fly` is removed, and the clamp
  reports the `providers` feature as disabled

#### Scenario: A job runs on stable

- **WHEN** the user runs `thegn job run cargo-test` in the stable channel with
  `[jobs] runner` naming a Sprites or Vercel env
- **THEN** the job runs on that runner instead of being refused as a
  dev-channel verb

#### Scenario: Other cloud verbs stay dev-only

- **WHEN** the user runs `thegn host list` or `thegn fleet list` in the stable
  channel
- **THEN** the command exits with the dev-channel error
