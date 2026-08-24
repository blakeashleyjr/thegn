## Why

thegn had no macOS install path that ends in something a user can launch. `install.sh`
deliberately opted darwin out of launcher integration — freedesktop `.desktop` entries are a
Linux/BSD concept — but nothing replaced it, so a Mac user got binaries on `PATH` and no entry in
Spotlight, Raycast, Alfred or the Dock. The Nix and Homebrew paths never run `install.sh` at all,
so they had no route to a launcher entry even in principle.

## What Changes

- A macOS `thegn.app` **launcher bundle**, generated locally by `packaging/macos/make-app.sh`:
  Info.plist, an owl `.icns`, a launcher executable, and a Terminal.app fallback runner.
- `install.sh` **detects the platform** instead of opting darwin out: `.desktop` + hicolor icon on
  Linux/BSD, `thegn.app` in `~/Applications` on macOS, binaries only elsewhere. Its summary now
  reports only the launcher files the platform actually received (it previously printed `.desktop`
  and icon paths on macOS, where neither was written).
- `just macos-app [bin] [dest]` generates the same bundle for the Nix and Homebrew installs, which
  never run `install.sh`.
- The bundle resolves a terminal emulator by **absolute path** (Ghostty → WezTerm → kitty →
  Alacritty → Terminal.app) and runs thegn through a **login shell**, because a GUI launch inherits
  launchd's environment: no nix profile, no Homebrew, no `~/.local/bin` on `PATH`, and a Mac
  terminal's CLI usually lives inside its `.app` rather than on `PATH`.
- `--env KEY=VALUE` bakes environment into a bundle, so a second side-by-side launcher can run a
  debug binary with `THEGN_LOG`/`THEGN_PERF` against an isolated `XDG_STATE_HOME`.
- `scripts/gen-owl-icns.py` renders `packaging/macos/thegn.icns` from the same `owl.rs` sprite as
  `config/thegn.svg`, in pure stdlib — no rasterizer and no `iconutil`, so it also runs on Linux.
  `just icons` regenerates both.

## Capabilities

### New Capabilities

- `macos-app-launcher`: how thegn registers itself with the macOS launcher — bundle generation, its
  inputs, terminal and binary resolution at launch time, and the Gatekeeper property that makes a
  locally generated bundle openable without code signing.

### Modified Capabilities

<!-- None: no existing spec covers installer/launcher behavior. -->

## Impact

- **Roadmap**: `tasks.md` AO.494 (single-command install) and AO.495 (NixOS module /
  home-manager) — this is the macOS half of "install ends in something launchable".
- **Code**: `packaging/macos/make-app.sh` (new), `packaging/macos/thegn.icns` (new, generated),
  `scripts/gen-owl-icns.py` (new), `install.sh` (platform detection + summary), `justfile`
  (`macos-app`, `icons`), `README.md`, `CHANGELOG.md`.
- **Distribution**: generating the bundle on the user's machine, rather than shipping a prebuilt
  one, is what keeps it free of `com.apple.quarantine`; a downloaded `.app` would require Developer
  ID signing plus notarization before it would open. This is the property the future Homebrew
  formula should preserve by generating the bundle at install time.
- **No new dependencies**: `bash` and (for regenerating the icon only) `python3` stdlib.
