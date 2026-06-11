# Follow-up polish: sandbox defaults, pane chrome, and navigation hints

Date: 2026-06-11

## Goal

Close the remaining usability gaps in the current visual/navigation pass:

- make the GPU stat glyph visually match the other masthead stats;
- start worktree environments automatically through the configured sandbox chain;
- show rootless/rootful podman, docker, bubblewrap, or host fallback status clearly;
- expose CPU/RAM/network stats for active containerized worktrees;
- make file access configurable while defaulting to safe build-cache mounts;
- remove the black halo/gap around the terminal focus ring;
- show bottom keybind hints for tab, pane, and worktree navigation;
- prevent closing or deleting the root/home workspace.

## Non-goals

- Build a full container orchestrator or compose UI.
- Hide host fallback. Host fallback is allowed, but it must be visibly labeled as uncontained.
- Make rootful podman require interactive sudo prompts inside the compositor.
- Change the core event-loop invariant. Startup, stats refresh, container probes, and backend detection must not block the render/input loop.

## Sandbox backend model

### Configurable preference chain

The sandbox backend order is configured by `[sandbox].backend_chain`. The default chain is:

1. `podman-rootless`
2. `podman-rootful`
3. `docker`
4. `bwrap`
5. `host`

`host` is a valid backend and the last default fallback. It means the worktree pane runs directly on the host and should be labeled as `HOST / UNCONTAINED` anywhere sandbox state is shown.

`backend = "auto"` resolves by walking `backend_chain`. A specific backend bypasses the chain for that worktree, except that if the backend cannot satisfy the configured mount/file-access policy, the resolver may fall back to the next backend in the chain when the worktree was created with `Auto`.

### New worktree sandbox picker

The new-worktree flow gets a sandbox picker with these options:

- `Auto` — use `[sandbox].backend_chain`;
- `Rootless Podman`;
- `Rootful Podman`;
- `Docker`;
- `Bubblewrap`;
- `Host`.

The default selected option is configurable, e.g. `[sandbox].default_backend = "auto"`. The selected value is persisted in the worktree registry row (`sandbox_backend`) and reused whenever panes for that worktree spawn. Existing rows without a value behave as `Auto`.

### Automatic startup

When an interactive pane is spawned for a worktree, superzej resolves the worktree's sandbox choice, ensures the backing environment exists, and then launches the shell through that environment. For OCI backends this means create/start/exec into a keep-alive container. For bubblewrap this means wrap each shell process directly. For host fallback this means spawn the host shell and mark the worktree as uncontained.

All backend detection, container creation, and stats probing must stay off the input/render loop. Failures are reported as pane/status messages rather than panics.

## File access and mounts

File access is policy-based and configurable.

### Required mount

Every sandboxed worktree gets its worktree path mounted read-write at the same path inside the sandbox. This keeps host-side git discovery and in-container tools aligned.

### Default safe caches

The default file-access mode is `worktree-plus-caches`. It mounts the worktree plus common build caches so developer workflows are fast without exposing `$HOME` or host `/` broadly.

Default auto-cache candidates include, when present:

- Cargo registry and git caches;
- rustup/toolchain paths when needed by the image/toolchain mode;
- npm, pnpm, yarn caches;
- Go module/build caches;
- pip/uv caches;
- Maven and Gradle caches;
- additional language caches that are explicitly added to the same policy mechanism.

Auto-cache mounts are best-effort. Missing cache paths are skipped. They are displayed as cache mounts in the SANDBOXES panel.

### Explicit mounts

Users can add mounts with host path, guest path, and mode:

```toml
[[sandbox.mounts]]
host = "~/.cargo/registry"
guest = "~/.cargo/registry"
mode = "rw"

[[sandbox.mounts]]
host = "~/.gitconfig"
guest = "~/.gitconfig"
mode = "ro"
```

Modes are `rw`, `ro`, and `cache`. `cache` is rendered as read-write cache storage but grouped separately in UI summaries.

### File-access modes

The config should support these modes:

- `worktree-only` — only the worktree mount;
- `worktree-plus-caches` — worktree plus auto caches and explicit mounts;
- `custom` — worktree plus explicit mounts only;
- `host` — host/uncontained behavior.

Backend translation:

- podman/docker: `--volume host:guest:rw|ro` plus the needed SELinux option when appropriate;
- bubblewrap: `--bind` or `--ro-bind`;
- host: no mount translation.

If a backend cannot enforce the requested policy, `Auto` tries the next configured backend. A specifically chosen backend reports the failure clearly.

## SANDBOXES panel

The panel's sandbox section shows one row per active worktree environment, prioritizing superzej-owned environments.

Each row should show:

- worktree or container name;
- selected/resolved backend;
- state (`running`, `starting`, `failed`, `host`);
- containment summary: `worktree`, `worktree+caches`, `custom mounts`, or `host/uncontained`;
- CPU, RAM, and network rx/tx when available;
- mount summary, e.g. `rw worktree · 5 cache · 1 ro`.

Stats sources:

- podman: `podman stats --no-stream --format json`;
- docker: docker stats equivalent;
- bubblewrap: no persistent container stats; show backend and mount policy only;
- host: show `HOST / UNCONTAINED` and omit container stats.

Stats refresh should integrate with the existing hydration/refresh path. Parsing lives in core so it is unit-testable.

## GPU stat glyph

The masthead stats cluster should render stat icons using a consistent visual slot. The GPU glyph is either replaced with a better-matching Nerd Font codepoint or padded/normalized so it occupies the same perceived width as CPU/RAM/network/battery.

The width budget must use the same normalized display width used for drawing. This prevents the responsive masthead from clipping or over-reserving when a font renders one glyph narrower than the others.

## Pane focus ring and black border

The focused pane card must own its entire rectangle. There should be no black margin outside the blue/white focus ring.

Implementation direction:

- fill the complete center/pane-card region with the intended chrome/pane background before composing terminal content;
- draw ring cells with that same background;
- ensure cells immediately inside and outside the ring are not left as terminal default black;
- preserve `pane_padding = 0` behavior so content sits flush to the ring when configured.

Expected result: the blue/white divider is the actual visible edge of the terminal card.

## Bottom keybind hints

The bottom statusbar should show focused, navigation-first hints. Hints should be derived from the effective keymap rather than hardcoded strings when possible.

Center focus hints prioritize:

- pane focus left/right/up/down;
- previous/next tab;
- previous/next worktree;
- new pane / split;
- zoom;
- command palette / lock.

Sidebar and panel focus hints include the relevant reset/back action plus the navigation actions that still apply. If the configured keymap changes, the displayed chord should change with it.

A small helper should map action IDs to the first effective chord and format via the keymap's hint display API. If an action has no binding, that hint is omitted instead of showing stale text.

## Root workspace protection

The root/home workspace is identified by `GroupKind::Home`, not by display name. It cannot be closed or deleted.

Required guards:

- hide close/delete actions from the context menu for home rows;
- bulk close/delete skips home groups;
- `CloseWorktree` refuses when the active group is home;
- delete confirmation text should not count skipped home groups as deletable;
- status messages explain `root workspace cannot be closed/deleted` when relevant.

This protection applies to both close-from-session and delete-from-disk flows.

## Error handling

- Sandbox resolution failure: show a pane/status error with backend attempts and reason.
- Container start failure: keep the worktree selected, show failed state in SANDBOXES, and do not panic.
- Explicit mount missing: report a warning/status; do not silently downgrade security.
- Auto-cache mount missing: skip quietly.
- Stats command missing/failing: leave stats blank and keep backend/status visible.
- Host fallback: label as uncontained.

## Testing plan

Core tests:

- backend-chain parsing and default order;
- rootless/rootful podman distinction;
- mount-policy expansion for `worktree-only`, `worktree-plus-caches`, and `custom`;
- OCI and bubblewrap argv generation does not expose host root/home unless configured;
- podman/docker stats parsing;
- host fallback renders as uncontained metadata.

Host tests:

- new-worktree sandbox picker default and persistence;
- active worktree pane uses sandbox launch path;
- home/root close and delete guards, including bulk actions;
- statusbar hints derive from configured keybindings;
- pane frame background cells around the focus ring are themed, not black;
- GPU stat cluster alignment/width regression.

Manual/smoke checks:

- rootless podman path starts a worktree container automatically;
- SANDBOXES shows podman backend plus CPU/RAM/network stats;
- switching worktrees reuses/persists the selected sandbox backend;
- host fallback is shown clearly when selected;
- no visual black halo around the terminal focus ring.
