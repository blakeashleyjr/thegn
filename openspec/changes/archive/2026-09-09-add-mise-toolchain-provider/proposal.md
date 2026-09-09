# Add mise as a first-class toolchain activation provider

Linear: THE-60

## Why

Detecting mise files and installing runtimes is insufficient if panes cannot
consume the resulting PATH/environment safely. The shipped provider makes mise
an explicit activation layer with local and remote parity, content-addressed
trust, and a cache-only launch path.

## Delivered design

- Local and remote detection cover mise project config, `conf.d`, `MISE_ENV`
  variants, `.tool-versions`, and common language pin files.
- `[toolchain.mise] inject = "auto" | "shims" | "env" | "off"` selects a
  provider-independent activation policy. Shims are the safe fallback;
  resolved environments require explicit Thegn approval.
- The approval identity hashes canonical detected paths and their contents,
  including `mise.lock` when present. Editing any input invalidates approval
  and cached activation.
- Thegn never runs `mise trust`. Its own repository-trust decision gates both
  `mise env` resolution and the explicit `mise install` operation.
- Environment resolution and remote probes/install preparation run off-loop.
  Launch consumes only an owner-only cache; cold remote/local state falls back
  safely and is warmed asynchronously.
- Activation precedence is bundle, devshell, mise, then base. Mise variables
  fill gaps only and credential-like keys are filtered.
- Doctor reports provider/tier/inject/state/trust/files/shims/reason from
  bounded cached/presence facts without resolving repo code on the diagnostic
  path.

## Non-goals

- Implementing a version manager, running mise tasks/hooks, or writing
  `mise.lock`.
- Installing mise itself on the host.
- Using mise's ambient trust store as Thegn's authorization source.
- Blocking a pane launch on toolchain detection, resolution, or installation.

## Evidence

Implemented in `envplan`, `toolchain_activation`, `toolchain`, and the host
`mise_provider` adapter, including local/remote cache and trust tests. Delivery
landed through the THE-60 commit series ending in `1bef1146`.
