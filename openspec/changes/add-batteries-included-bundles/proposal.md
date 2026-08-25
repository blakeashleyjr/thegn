# Batteries-included editions: terminal + font + deps, composed not embedded

Linear: THE-15

## Why

THE-15 asks for "distributions for each platform that come with a fully
featured terminal, fonts, all deps". The honest first question is whether
shipping a terminal is in thegn's lane at all — thegn is a TUI that renders
_into_ the user's terminal, and `terminal-compat` exists precisely so it
degrades gracefully in whatever emulator it finds. The answer this change
takes: **owning an emulator is out of the lane; composing one is squarely in
it.** Forking or embedding a terminal makes thegn the permanent maintainer of
an escape-parser/font-shaping/GPU attack surface, and contradicts the lane
`define-gui-frontend-lane` (THE-40) pins — never a second render backend. But
a _distribution_ that pins an upstream emulator, a Nerd Font, and the runtime
tools around the existing binary is packaging, the same lane as
`macos-app-launcher` and `nix/package.nix`.

Most of the raw material already exists, scattered and unguaranteed:

- **Bundled terminal profiles**: `config/alacritty.toml` and
  `config/ghostty.config` are hermetic profiles (option-as-alt,
  `xterm-256color` TERM, padding) that both name **FiraCode Nerd Font** —
  which nothing installs. If the font is absent the profile silently falls
  back and glyphs degrade.
- **A standalone window**: `tg --standalone` (install.sh) opens thegn in its
  own alacritty window on the bundled profile, and the Linux `.desktop` entry
  launches it — but it errors out if alacritty is not already installed. The
  in-app font picker patches that same profile (`THEGN_ALACRITTY_CONFIG`).
- **The macOS launcher** (`macos-app-launcher` spec): a locally generated
  `thegn.app` that resolves whatever terminal the user has, falling back to
  Terminal.app — the worst-fidelity candidate (no option-as-meta by default,
  no bundled profile).
- **Deps**: guaranteed only on the nix path (`nix/package.nix` wraps
  git/fzf/gum/lazygit/yazi/delta/gh + yazi previewers onto PATH). Tarball
  installs get `thegn doctor` probes and nothing else.

So today there is no artifact or command that yields the guaranteed
experience: truecolor + full Nerd-Font glyphs + undercurl + working Alt
chords + every runtime tool present. That guarantee is the product THE-15 is
actually asking for.

## What Changes

- **The lane rule, spec'd.** Batteries editions SHALL compose pinned upstream
  emulators and fonts; thegn never forks, vendors, or embeds one. The only
  thegn-owned artifacts are the bundled profiles and wrappers.
- **`.#batteries` — the reference edition (nix).** A flake output composing a
  pinned alacritty (nixpkgs, linux + darwin), FiraCode Nerd Font (scoped via
  fontconfig, not installed user-wide), the bundled profile, and the
  already-wrapped thegn with its pinned runtime tools. Launching it on a
  clean host requires no preinstalled terminal, font, or tool. Zero recurring
  release cost — `flake.lock` owns every pin.
- **macOS batteries mode — provision at install time, stay unsigned.**
  `install.sh --batteries` / `just macos-app --batteries` ensures a preferred
  emulator + the Nerd Font via what is available (nix-darwin / brew casks),
  points the generated `.app` at the provisioned emulator **with the bundled
  profile**, and reports precisely what it could not provision. The bundle
  stays locally generated — the standing Gatekeeper rule
  (`macos-app-launcher`) and the RELEASING.md no-notarization decision are
  reaffirmed: no downloadable mac app bundle unless signed and notarized.
- **Fidelity as a contract.** A batteries launch SHALL land in an environment
  where `terminal-compat` detection resolves truecolor + full glyphs +
  undercurl and Alt chords deliver — checkable via `thegn doctor` /
  `just term-check`, not a marketing adjective.
- **Deferred platforms get entry criteria** (mirroring
  `add-package-manager-releases`): Windows batteries behind the windows-msvc
  release leg (`add-windows-ci-distribution`) — Windows Terminal is stock on
  Win11, so the shape is a profile + winget dependency story decided inside
  that track; portable Linux formats (flatpak/AppImage/`nix bundle`) deferred
  with the GL/driver-portability problem (nixGL class) recorded as the
  blocker. Batteries editions ship the **stable** channel only, per the
  sibling change's channel rule.

## Non-goals

- Writing, forking, vendoring, or embedding a terminal emulator.
- Any GUI frontend (owned by `define-gui-frontend-lane`; a batteries edition
  is packaging around the existing TUI, not a frontend).
- macOS code signing / notarization (RELEASING.md decision unchanged); hence
  no downloadable mac `.dmg`/`.app` artifact.
- flatpak/AppImage/snap artifacts now (entry criteria recorded in the spec).
- New config keys, new CLI verbs, or any change to the render path,
  event loop, or control plane.

## Impact

- Roadmap: **AO 493** (sane out-of-box defaults), **AO 494** (single-command
  install), **N 175/176** (font config, Nerd Font / icon support). A 5
  (single-binary distribution) is unchanged: batteries is an additive
  optional edition; the plain single binary remains the product.
- Specs: **`distribution`** (4 ADDED requirements: composition lane rule, the
  nix reference edition, the fidelity contract, platform staging) and
  **`macos-app-launcher`** (1 ADDED requirement: install-time batteries
  provisioning).
- Code: `flake.nix` (+ a small `nix/batteries.nix`), `install.sh`,
  `packaging/macos/make-app.sh`, README install matrix + `docs/help/` install
  prose. **No Rust changes, no capability-catalog row** — packaging is
  out-of-process; nothing here opens a door into a running instance (same
  precedent as `add-package-manager-releases`).
- In-flight reconciliation: `add-package-manager-releases` (THE-52, sibling:
  batteries builds on its artifact contract + verified-before-advertised rule
  and its `distribution` capability); `add-release-channels` (stable-only
  packaging); `add-windows-ci-distribution` (windows batteries staged behind
  its msvc leg); `define-gui-frontend-lane` (the composition rule here is the
  packaging-side face of its "no second render backend" pin);
  `add-terminal-presets` is unrelated (in-app launch shapes, not
  distribution).
