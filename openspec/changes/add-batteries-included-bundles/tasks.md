# Tasks

## 1. Nix reference edition

- [ ] 1.1 `nix/batteries.nix`: wrapper package composing pinned alacritty +
      FiraCode Nerd Font (generated fontconfig, `FONTCONFIG_FILE` in the
      wrapper) + the bundled profile + the existing wrapped `thegn`;
      first-launch copy of the profile into `$XDG_CONFIG_HOME/thegn/` so the
      font picker has a writable file (`THEGN_ALACRITTY_CONFIG` points at
      it).
- [ ] 1.2 `flake.nix`: expose `.#batteries` (package + app); stable channel
      binary only. Keep `.#default` untouched.
- [ ] 1.3 Verify on a clean host (fresh VM or empty-profile user): launch
      with no preinstalled terminal/font/tools; record a `thegn doctor` run
      from inside the batteries window showing truecolor + full glyphs +
      undercurl and all runtime tool probes green.

## 2. macOS batteries provisioning

- [ ] 2.1 `install.sh --batteries` (Darwin path): detect an acceptable
      emulator + the Nerd Font; provision missing pieces via brew casks
      (`alacritty`, `font-fira-code-nerd-font`) with explicit confirmation,
      or print the exact nix-darwin/brew steps and exit nonzero without
      registering a launcher. Per-item provisioned/present/missing summary;
      `--dry-run` prints the plan.
- [ ] 2.2 `packaging/macos/make-app.sh` + `just macos-app`: accept
      `--batteries` / a preferred-emulator override so the generated bundle
      pins the provisioned emulator with the bundled profile (extending the
      existing `--alacritty-config` plumbing). Bundle stays locally
      generated — no distribution of the `.app`.
- [ ] 2.3 Rehearse end-to-end on a Mac (brew-present and brew-absent paths)
      and record the doctor output, mirroring 1.3.

## 3. Linux installer tier

- [ ] 3.1 `install.sh --batteries` (Linux path): verify emulator + font;
      provision via a recognized distro package manager where one is
      (pacman/apt/dnf named-package attempt with confirmation), else print
      the exact packages and exit nonzero. The existing `.desktop` →
      `tg --standalone` wiring is unchanged.
- [ ] 3.2 Replace the bare "alacritty not found" failure in `tg
--standalone` with a pointer to `install.sh --batteries` (or the nix
      output) — message-only change in the generated wrapper.

## 4. Docs + staging

- [ ] 4.1 README install matrix: add the batteries rows (nix `.#batteries`;
      macOS/Linux `install.sh --batteries`) only after their rehearsals
      (1.3, 2.3) are recorded — the verified-before-advertised rule from
      `add-package-manager-releases` governs.
- [ ] 4.2 Record the deferred platforms with entry criteria, not
      instructions: Windows batteries behind `add-windows-ci-distribution`'s
      msvc leg (shape: Windows Terminal profile fragment + winget
      dependency story); flatpak/AppImage/`nix bundle` behind the GL/driver
      portability question and actual demand.
- [ ] 4.3 Mirror the essentials in `docs/help/` install prose if the help
      corpus carries an install page (no new action ids — help ratchets
      unaffected).
- [ ] 4.4 Note the font-picker/ghostty parity gap where the picker is
      documented, pointing at the open question in design.md.

## 5. Validation

- [ ] 5.1 Run `just ci` once, when the implementation is complete (includes
      `openspec validate --all --strict`; the nix output is additionally
      proven by `nix build .#batteries` in the flake-check lane, and the
      installer paths by the recorded rehearsals, since they need real
      hosts).
