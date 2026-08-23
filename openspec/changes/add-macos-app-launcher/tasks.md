## 1. Icon

- [x] 1.1 Write `scripts/gen-owl-icns.py`: render the `owl.rs` sprite at every `.icns` size from the
      pixel data (stdlib only — no rasterizer, no `iconutil`, so it runs on Linux/CI too)
- [x] 1.2 Generate + commit `packaging/macos/thegn.icns` so an install needs no Python
- [x] 1.3 Add `just icons` to regenerate both `config/thegn.svg` and the `.icns` from the sprite

## 2. Bundle generator

- [x] 2.1 Write `packaging/macos/make-app.sh` with `--bin`, `--dest`, `--name`, `--icon`,
      `--terminal`, `--alacritty-config`, `--dry-run`
- [x] 2.2 Emit `Info.plist` (full version in `CFBundleShortVersionString`, numeric-dotted in
      `CFBundleVersion`), `PkgInfo`, and the `.icns`; deliberately no `LSUIElement`
- [x] 2.3 Emit the bundle executable: resolve the binary (baked path, then known install prefixes),
      resolve a terminal by absolute `.app`/prefix path, exec it running thegn via a login shell
- [x] 2.4 Emit the Terminal.app `.command` runner as the always-available fallback
- [x] 2.5 Surface failures through an `osascript` alert — a GUI launch has nowhere to print
- [x] 2.6 Add repeatable `--env KEY=VALUE`, exported in the launched session
- [x] 2.7 Re-register with LaunchServices (`lsregister -f`) so a regenerated bundle is picked up
      without a re-login

## 3. Install paths

- [x] 3.1 Replace `install.sh`'s darwin opt-out with platform detection: `.desktop` on Linux/BSD,
      `thegn.app` on macOS, binaries only elsewhere
- [x] 3.2 Fix the `install.sh` summary to report only the launcher files the platform received (it
      printed `.desktop`/icon paths on macOS, where neither is written)
- [x] 3.3 Add `just macos-app [bin] [dest]` for the Nix and Homebrew paths, which never run
      `install.sh`

## 4. Verification

- [x] 4.1 Verify on a real Mac: bundle generates, Spotlight indexes it
      (`mdfind kMDItemCFBundleIdentifier`), launching opens Ghostty with the login-shell environment
- [x] 4.2 Verify the `exec` handoff leaves no windowless thegn app registered (`lsappinfo list`),
      confirming `LSUIElement` is unnecessary
- [x] 4.3 Verify the generated scripts are syntactically valid and `Info.plist` passes `plutil -lint`
- [x] 4.4 `shellcheck -x` + `treefmt` clean on the new and edited shell scripts
- [x] 4.5 Extend `test/install-plan.sh` (the `just smoke` gate) to cover the macOS branch: the
      dry-run plans the bundle and no `.desktop`, and a real install produces a bundle whose
      launcher points at the INSTALLED binary

## 5. Docs

- [x] 5.1 README: macOS app-launcher section (generation, terminal resolution, the Gatekeeper
      property) and correct the stale "macOS has never been compiled" claim
- [x] 5.2 CHANGELOG entry under Unreleased

## 6. Pre-PR gate

- [ ] 6.1 `just ci` (or the pre-push gate: clippy + `cargo test` + smoke) green before this lands
