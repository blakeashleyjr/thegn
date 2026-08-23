## Context

thegn is a TUI, so "launch it" means "open a terminal emulator running it". On Linux that is
expressible in a `.desktop` file (`Exec=tg --standalone`, `Terminal=false`), which is why
`install.sh` has always written one. macOS has no equivalent text-file registry: the unit
Spotlight, Raycast, Alfred and the Dock index is an `.app` bundle. `install.sh` opted darwin out of
launcher integration rather than build one, and the Nix and Homebrew install paths never run
`install.sh` at all, so neither had a route to a launcher entry.

Two macOS-specific constraints shape the design:

1. **A GUI launch has launchd's environment**, not a shell's. It does not include nix profiles,
   `/opt/homebrew/bin`, or `~/.local/bin` — and on a Mac the terminal emulators themselves usually
   ship as `.app` bundles whose binary is inside `Contents/MacOS`, not on `PATH`. Anything resolved
   by `command -v` at launch time finds nothing.
2. **Gatekeeper treats downloaded bundles differently from locally created ones.** A `.app` that
   arrives over the network carries `com.apple.quarantine` and is refused unless signed with a
   Developer ID and notarized. A bundle written by a local process carries no such attribute.

## Goals / Non-Goals

**Goals:**

- A macOS user who installs thegn by any route (source, Nix, Homebrew) can reach it from their
  launcher.
- The bundle keeps working when the underlying binary is upgraded or moved.
- No signing certificate, no notarization, no Apple Developer account.
- One generator, reused by every install path, so the paths cannot drift.

**Non-Goals:**

- Shipping a prebuilt `.app` as a release asset or Homebrew Cask (that needs signing +
  notarization; see Risks).
- A native macOS GUI. The bundle is a launcher that hands off to a terminal emulator and exits.
- Windows launcher integration (Start-menu shortcut). Out of scope here.

## Decisions

**Generate the bundle locally; never ship one.** This is the decision the Gatekeeper story hangs
on: a locally generated bundle has no quarantine attribute and opens with no signing. The
alternative — shipping a `.app` in the release and telling users to `xattr -dr com.apple.quarantine`
— trains people to strip a security control, and a Cask would need real notarization. The cost is
that the bundle must be regenerated after the binary moves; the launcher's prefix search (below)
absorbs the common cases.

**Resolve everything by absolute path at launch, and go through a login shell.** The bundle bakes
the binary path it was generated against, then falls back to searching known prefixes
(`~/.nix-profile/bin`, `/etc/profiles/per-user/$USER/bin`, `/run/current-system/sw/bin`,
`~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `~/.cargo/bin`). Terminal emulators are
probed at their `.app` binary paths first, then those same prefixes. Running thegn as
`<terminal> -e <login-shell> -l -c 'exec <thegn>'` is what gives the process the `PATH` an
interactive terminal would have — thegn shells out to `git`, `gh`, `fzf`, `lazygit` and `delta`,
and a launchd-inherited environment has none of them.

**`exec` into the terminal rather than `open -na`.** Verified on a real Mac: after the exec,
LaunchServices tracks the terminal and no windowless thegn process lingers in the Dock. Because of
that, the bundle is deliberately **not** marked `LSUIElement` — marking it an agent would buy
nothing and some launchers hide agent apps.

**Render the `.icns` from the sprite, in stdlib Python.** `config/thegn.svg` is generated from
`crates/thegn-host/src/owl.rs`, and the sprite is axis-aligned 10×10 blocks, so every icon size can
be rendered directly from the pixel data instead of scaling a bitmap — small sizes stay crisp.
Writing the `.icns` container directly (rather than shelling out to `iconutil`) keeps the generator
runnable on Linux/CI, and needing no rasterizer means no `librsvg`/ImageMagick dependency. The
result is committed so an install needs no Python at all.

**Terminal preference order, with Terminal.app as the floor.** Ghostty → WezTerm → kitty →
Alacritty → Terminal.app, overridable with `--terminal`. Terminal.app is on every Mac, so the
launcher always has a working answer; it is reached through a generated `.command` file, which
Terminal runs in a login shell of its own.

**Failures must be visible.** A GUI launch has no stdout, so an unresolvable binary raises an
`osascript` alert naming the fix rather than bouncing the icon once and dying.

## Risks / Trade-offs

- **The bundle points at a binary that later moves** (nix profile rebuild, `cargo clean`) →
  Mitigated by the prefix search at launch, and by an alert naming the regeneration command when
  even that fails. Re-running `just macos-app` is the fix.
- **Bundle regeneration is manual for the Nix/Homebrew paths** → Accepted for now; the future
  Homebrew formula should call the generator in its install step, which also preserves the
  no-quarantine property.
- **`exec`-ing another app's binary is not a documented LaunchServices contract** → Verified
  empirically (window opens, correct env, no stale app registration). The Terminal.app path uses
  the fully supported `open -a` route, so there is a working floor if a future macOS changes this.
- **Icon fidelity**: the renderer draws the plate as a rounded rectangle rather than Apple's exact
  superellipse, and skips the hairline border below 128px → Cosmetic; the icon is legible at every
  size the `.icns` carries.

## Migration Plan

Additive: no existing behavior changes on Linux/BSD, and no state or config is touched. A macOS
user re-runs `./install.sh` (or `just macos-app`) once to gain the launcher. Rollback is deleting
`~/Applications/thegn.app`.

## Open Questions

- Should the Homebrew formula generate the bundle in a `post_install` hook once the tap exists, or
  leave it to `just macos-app`? Generating it is friendlier and keeps the no-quarantine property,
  but adds a step Homebrew audits dislike.
- Should `install.sh` offer `/Applications` (all users) as well as `~/Applications`? Writing there
  needs `sudo`, which the installer otherwise never uses.
