# Chunk 3 — Nix batteries edition and verified install documentation

## Scope

Add the smallest honest THE-15 deliverable: a Nix composition that launches
the existing stable package inside pinned Alacritty with FiraCode Nerd Font
available. Keep the default/dev packages and existing Home-manager config
schema unchanged. Document only what can be proven locally; record all
host/account-dependent ideas as deferred entry criteria.

## Exact files touched

- `nix/batteries.nix` (new)
- `flake.nix`
- `nix/source.nix`
- `README.md`
- `docs/help/terminal-compatibility.md`

No other chunk may edit these paths. This chunk does not edit `install.sh`,
`packaging/macos/make-app.sh`, `RELEASING.md`, or any `Cargo.toml`.

## Approach

1. Add `nix/batteries.nix` as a separate Nix module/function, not more logic in
   the flake's already large `outputs` expression. It receives the current
   system package set, the existing stable wrapped package, the bundled
   Alacritty profile, and pinned upstream packages.
2. Compose `pkgs.alacritty` and the current Nixpkgs Nerd Font package for
   FiraCode. Generate a fontconfig file scoped to the launcher; do not install
   fonts into the user's global/system font directory and do not copy any
   third-party binary into a release archive.
3. Produce a `thegn-batteries` launcher package. On launch, create a user-owned
   `$XDG_CONFIG_HOME/thegn/alacritty.toml` only when absent, copy the immutable
   `config/alacritty.toml` there, export `THEGN_ALACRITTY_CONFIG`, set the
   fontconfig environment, and exec Alacritty with `--config-file` and the
   existing stable wrapped `thegn`. Preserve arguments and quote every path.
   The wrapper must not open SQLite during build or use a live state directory.
4. Expose `packages.<system>.batteries` and make `nix run .#batteries` resolve
   its launcher through `meta.mainProgram`. Keep `.#default`, `.#thegn`, and
   `.#dev` behavior untouched. Do not add an `apps` output unless evaluation
   proves it is required by the existing flake contract.
5. Add `nix/batteries.nix` to the explicit source allowlist in
   `nix/source.nix`; keep all profile/config references under already
   allowlisted `config/**`. If the implementation adds another build-time file,
   add that exact path to the allowlist in the same commit.
6. Document `nix run .#batteries` and the Home-manager composition using the
   existing `programs.thegn.package`/`home.packages` mechanism. Do not add a
   `batteries` runtime config key, HM option, action id, help context, or
   package-manager claim that depends on an unrecorded host rehearsal.
7. In the terminal compatibility help, explain that batteries is a composed
   Nix path and that `thegn doctor` is the verification command. State that
   Ghostty remains a shipped profile but is not the pinned batteries emulator,
   and that the macOS font-picker/alternate-emulator parity remains deferred.
   Keep frontmatter/actions unchanged.
8. Explicitly document the cuts: no `install.sh --batteries`, no distro
   package-manager mutation, no Homebrew cask, no downloadable unsigned macOS
   app, no Windows Terminal profile, and no Flatpak/AppImage/nix-bundle until
   their host/signing/driver rehearsals exist.

## Tests to run

- `nix build .#batteries`
- `nix build .#thegn-nobridge`
- `just quick thegn-host` (scoped source sanity; no Rust code should change)
- `cargo nextest run -p thegn-host --lib` (scoped regression check; if the
  wrapper is exercised, set `XDG_STATE_HOME` to a temporary directory)
- `python3 -m unittest discover -s packaging/tests -p 'test_*.py'` to ensure
  documentation/Nix changes did not alter the release contract
- `git diff --check`

Do not run `just ci`, `just test`, full-workspace builds, or e2e. A clean-host
interactive batteries rehearsal is a follow-up evidence task; it is required
before calling the path “verified” in user-facing install matrices.

## Dependencies / overlap

Chunk 3 is file-disjoint and independent of Chunks 1 and 2. It may be
parallelized with Chunk 1. The only logical follow-up is evidence collection
after `nix build`; no implementation dependency exists.

## Done criteria

- `nix build .#batteries` evaluates and builds on every system already declared
  by `flake.nix`; the wrapper launches only stable `thegn`, with pinned
  Alacritty, FiraCode Nerd Font, bundled profile, and existing runtime tools.
- First launch creates only the documented user-scoped writable profile copy;
  subsequent launches reuse it and the font picker points at the same file.
- The default/dev Nix outputs, source allowlist discipline, and all runtime
  ratchets remain intact. `config/config.toml.example`, env-overlay,
  completion-slot, control-schema, and help-ratchet files are unchanged because
  no runtime key/surface/action is introduced.
- README/help advertise the batteries command only with a clear verification
  status and list deferred forms as entry criteria, not install instructions.
- Commit the chunk with exactly:

  `feat(nix): add thegn batteries launcher`
