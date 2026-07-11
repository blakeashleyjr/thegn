# Yazi OOM Prevention Design

Date: 2026-06-11

## Problem

Thegn-spawned yazi drawers can launch image preview helper processes such as `ueberzugpp`. On this host, several yazi/ueberzugpp process trees consumed nearly all RAM and swap, causing a global OOM kill and leaving the desktop/session unstable. `thegn` itself was small in the OOM table; the failure mode is unbounded helper processes spawned by thegn-managed drawers.

The drawer is convenient but too trusting: thegn-spawned yazi is launched as a normal child process, and image preview helpers can grow outside any product-owned resource boundary. In the zellij-backed branch this applies to both `thegn files` and `thegn tool yazi`; in the native host branch it also applies to pooled/prewarmed drawer panes.

## Goals

- Thegn-spawned yazi must not be able to exhaust host RAM or swap.
- Image previews stay off by default in thegn's bundled/private yazi config.
- Text/code previews remain available by default.
- Users can explicitly opt into image previews and tune limits.
- Every thegn-spawned yazi path is wrapped in a product-owned resource boundary when systemd-run is available.
- Native-host hidden drawer pooling is bounded so invisible yazi instances cannot grow without limit.
- The solution covers current zellij launch paths (`thegn files`, `thegn tool yazi`) and the native host drawer/prewarm model.

## Non-goals

- Do not parse kernel logs or infer exact OOM causes in the first implementation.
- Do not edit the user's system yazi config when `drawer.config_home = "system"`.
- Do not remove the yazi drawer feature.
- Do not add polling or blocking work to the event loop.

## Architecture

Treat the yazi drawer as a managed tool, not a raw command. All drawer launch paths flow through one helper that builds the yazi environment, builds the yazi argv, optionally wraps it in a resource containment argv, then calls the existing PTY spawn path.

Protection has three layers:

1. Safe bundled config. The bundled/private yazi config disables image preview backends by default while keeping text/code previews enabled.
2. Resource containment. When enabled and available, yazi runs through a user systemd transient scope with memory, swap, and CPU properties. Yazi preview helpers inherit the scope, so a leak kills the drawer scope rather than the desktop.
3. Bounded lifecycle. Current zellij drawers are spawn-on-open/close-on-dismiss, so they do not keep a hidden pool. Native pooled drawers use the same config surface with a maximum pool size and prewarm disabled by default.

## Configuration

Extend `[drawer]` with safety controls:

```toml
[drawer]
command = ""
config_home = ""
height = "35%"
width = "full"

# Image previews are off by default in the bundled yazi config. Set true to opt in.
image_previews = false

# Contain yazi and preview helper children. Empty limit strings omit that property.
contain = true
memory_max = "2G"
memory_swap_max = "512M"
cpu_quota = "200%"

# Hidden drawer lifecycle.
pool_limit = 1
prewarm = false
```

Default behavior is safe and boring: no image helpers from the bundled config, hard process-tree limits when systemd-run is usable, no current zellij hidden drawer pool, and no automatic yazi prewarm in native hosts.

`image_previews = false` only controls thegn's bundled/private yazi config. If the user selects `config_home = "system"`, thegn does not rewrite the user's yazi config; containment remains the safety net.

## Launch flow

```text
yazi action
  -> resolve active or selected directory
  -> seed/select yazi config and env
  -> build yazi command argv/script
  -> wrap with containment when drawer.contain is true and systemd-run is available
  -> spawn the drawer/tool pane
  -> record visible drawer state when applicable
```

The PTY and emulator implementation stay unchanged. The wrapper must not request a second pseudo-terminal from systemd; thegn already owns the PTY. The containment command should run inside the existing PTY and exec the configured yazi command under the transient scope.

## Exit and cleanup

When a pane exits, the event loop already receives `PaneEvent::Exit(id)`. Drawer handling should additionally:

- clear the visible drawer if its pane exited;
- remove any pooled drawer entry for the exited id;
- avoid restoring dead drawer ids;
- set persisted drawer state to closed for the matching worktree when known;
- show a concise status when a visible drawer exits, e.g. `Files drawer exited; if previews were enabled, it may have hit the drawer memory limit.`

This cleanup is event-driven and must not poll.

## Pooling and prewarm

The current zellij-backed drawer has no hidden keepalive pool: dismissing the drawer closes the pane, and restore only reopens drawers explicitly persisted as open. The implementation therefore adds `pool_limit` and `prewarm` as safe defaults for the native host path without applying them to zellij.

For native hosts, `DrawerPool` becomes bounded. `pool_limit = 0` means hiding kills the drawer instead of stashing it. `pool_limit = 1` keeps only the most recently hidden drawer. Larger values are explicit user choice.

When native stashing would exceed the limit, the oldest hidden drawer id is evicted and removed from `Panes`, terminating that PTY child tree through the normal pane drop behavior.

`drawer.prewarm = false` disables invisible yazi prewarming by default. If a user enables it in a native host, prewarming still respects `pool_limit` and containment.

## Error handling

- If `drawer.contain = true` but `systemd-run` is missing or unusable, launch yazi directly, log a warning, and surface a status message where practical.
- Empty `memory_max`, `memory_swap_max`, or `cpu_quota` strings omit only that property.
- If all limit strings are empty but containment is enabled, still using a scope is acceptable for process grouping and cleanup.
- If a configured drawer command fails to spawn, leave drawer state unchanged and surface a status when the caller can report it.

## Testing

Core/config tests:

- drawer defaults include `image_previews = false`, `contain = true`, `memory_max = "2G"`, `memory_swap_max = "512M"`, `cpu_quota = "200%"`, `pool_limit = 1`, and `prewarm = false`;
- TOML overrides parse for all new drawer fields;
- partial drawer config preserves the safe defaults.

Host tests:

- containment argv includes `systemd-run --user --scope` and non-empty limit properties;
- containment argv omits empty properties;
- containment disabled leaves argv unwrapped;
- yazi env/config uses bundled private config by default;
- system config selection does not rewrite user config;
- image-preview settings change the seeded bundled config deterministically;
- the current zellij implementation has no hidden drawer pool/prewarm sites; native host pool behavior is covered when that code is present.

Manual verification:

- With default config, open/close the files drawer across several worktrees and confirm no `ueberzugpp` process is spawned.
- Enable image previews and browse image-heavy directories; confirm yazi/preview helpers live under a user systemd scope with the configured memory/swap/CPU properties.
- Force a low memory limit and confirm only the drawer dies, not the terminal or `thegn`.

## Success criteria

A default thegn session can use yazi drawers/tools without launching image preview helpers from the bundled config, cannot accumulate native hidden yazi panes without an explicit limit, and cannot let an explicitly preview-enabled yazi process tree consume unbounded RAM/swap when systemd-run containment is available.
