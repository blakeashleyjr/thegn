---
id: sandboxing
title: Sandboxing
parent: workflows
order: 3
contexts: [panel:sandbox, panel:environments]
actions: [warm-pool-increment, warm-pool-decrement]
---

# Sandboxing

Each worktree's interactive process — usually a coding agent — can run in
a container while the worktree itself stays on the host, bind-mounted at
its real path, so host-side git (the [[panel]] diff, the [[sidebar]]
state) keeps working.

## Backends

Chosen automatically in order: `podman` → `docker` → `bwrap` → `none`;
pin one with `[sandbox] backend`. A remote backend runs worktrees on
another machine over SSH.

## Choosing per worktree

The `Alt-w` "what to run" picker offers the sandbox choice when
`[sandbox]` is configured; `[[agents]]` entries can pin their own. A
repo-root `.thegn.toml` can override sandbox settings per repo.

## Inspecting

The [[panel]]'s **system → sandbox** section shows the live sandbox state
for the focused worktree: backend, image, mounts, DNS filtering. `thegn
doctor` reports what backends are available on this machine.

## Environments

Named `[env.<name>]` execution environments (local, container, or a cloud
provider) appear in the panel's **system → environments** section and the
palette's "New environment…" wizard. An environment can keep a **warm
spare pool** so new worktrees start instantly; raise or lower the active
workspace's pool target with the warm-pool actions (palette-runnable,
bindable).

See the [[config-reference]] `[sandbox]` tables for images, mounts, CPU
caps, prefetch, and the DNS allowlist.
