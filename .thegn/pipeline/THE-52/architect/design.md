# THE-52 / THE-15 architecture

## Decision

Build one pure, offline release renderer around two inputs:

1. `Cargo.toml` `[workspace.package].version` is the version authority.
2. `packaging/release.json` is the release-manifest authority for artifact
   names, target availability, package metadata, runtime dependency names, and
   the stable-channel policy.

The renderer validates a tag, renders all package-manager metadata into a
throwaway directory, and emits deterministic package inputs. The tag workflow
builds the existing archives, attests each archive, invokes the renderer, and
uploads the rendered metadata plus `.deb`/`.rpm` assets. Publishing to the
Homebrew tap and AUR remains an explicit, separately enabled release step: the
required accounts and write credentials are external state and cannot be
invented in this repository. The workflow must print a visible checklist when
that step is not enabled.

The batteries deliverable is narrower: a Nix `.#batteries` launcher package
composing the existing wrapped stable `thegn`, pinned Alacritty, FiraCode Nerd
Font, and the existing writable-profile convention. It does not embed a
terminal or font in a tarball, change `thegn-core`, add an installer-side
package-manager transaction, or create an unsigned downloadable macOS app.
Home-manager users select the package explicitly through the existing package
option; no new runtime configuration key is needed.

## Branch audit and draft reconciliation

The two OpenSpec changes are useful drafts, but several claims are not true of
this branch and are pruned here.

Already satisfied on this branch:

- The tagged workflow creates a draft release and uploads Linux GNU, Linux
  musl, and Apple-silicon archives with checksums. The archive naming, root
  layout, licenses, and README are implemented in
  `.github/workflows/release.yml:1-13,68-113` and rehearsed by
  `justfile:1094-1156`.
- Nix already builds the stable package, a dev package, and the runtime tool
  closure. `flake.nix:563-603` exposes the packages and
  `nix/package.nix:47-73,145-159` wraps stable `thegn` with git, fzf, gum,
  lazygit, yazi, delta, gh, coreutils, and yazi preview dependencies.
- Cargo-binstall metadata already points at the release contract and already
  has the Windows archive-format override in
  `crates/thegn-host/Cargo.toml:20-29`. It is inert until the workspace's
  `publish = false` policy changes; this issue does not publish the workspace
  to crates.io.
- Shell completions are already a single, isolated release asset in
  `.github/workflows/release.yml:114-190`; the packaging work must not put a
  second copy into every archive.
- The release target gate is already explicit: Windows is not in the release
  matrix (`.github/workflows/release.yml:15-31,68-84`), while the opt-in CI job
  only uploads a per-run artifact (`.github/workflows/ci.yml:394-436`).

The draft items that are not implemented and are covered by this design:

- `packaging/homebrew/thegn.rb` is a hand-edited concrete formula with
  `version` and `REPLACE_WITH_AARCH64_SHA256`
  (`packaging/homebrew/thegn.rb:15-30`), not a render template.
- There is no `packaging/aur/`, no Scoop/winget metadata, no `.deb`/`.rpm`
  producer, and no provenance step in `release.yml`.
- `install.sh` accepts only `--dry-run` (`install.sh:12-19,30-54`), always
  builds from source (`install.sh:106-124`), and its standalone wrapper fails
  when Alacritty is missing (`install.sh:161-167`). It has no batteries path.
- There is no `nix/batteries.nix` and no `apps` output; current flake outputs
  are packages/modules only (`flake.nix:563-603,780-790`). The draft's claims
  that a batteries output, macOS provisioning, rehearsals, and a cask already
  exist are therefore not evidence.
- The draft explicitly deferred `.deb`/`.rpm`, but THE-52 asks for an
  evaluation and a release-produced result. This design includes standalone
  nfpm assets, not a hosted apt/rpm repository.

The draft's broad batteries plan is cut where it depends on unverified host
behavior: `install.sh --batteries`, distro-specific package transactions,
Homebrew cask installation, downloadable macOS bundles, Windows Terminal
integration, and Flatpak/AppImage/nix-bundle. Those are documented as entry
criteria, not represented as green install instructions.

## Invariants and boundaries

This is packaging/infrastructure work, outside the running application:

- No Rust source, `thegn-core` dependency, provider seam, capability-catalog
  row, control API, keymap, database migration, or config key changes.
- Therefore `config/config.toml.example` remains unchanged. The
  env-overlay, completion-slot, control-schema, platform, provider, and help
  ratchets must remain byte-for-byte unchanged; each coder verifies the
  relevant scoped tests in the same chunk and must not add an excuse line.
- No work touches the event loop, render decision, terminal degradation
  chokepoints, or host platform `cfg` boundaries. The renderer is a pure
  process-level script; it does not become a `thegn-core` or host module.
- Generated files are never committed to this repository as mutable release
  state. Templates are committed; generated formula/PKGBUILD/manifests and
  nfpm inputs are release outputs. `.SRCINFO` is generated by `makepkg
--printsrcinfo`, never hand-edited.
- All release builds remain `--locked`. A package manager receives the stable
  binary (`thegn`/`tg`), including prerelease versions whose runtime channel is
  stable; no `thegn-dev` package is rendered.
- Nix source remains an allowlist. The new evaluator input
  `nix/batteries.nix` is added to `nix/source.nix` explicitly, and it may only
  reference already allowlisted `config/**` content and Nix package inputs.
  This avoids the common “works in evaluation, missing from the sandboxed
  source” trap described by `nix/source.nix:11-21`.

## Release manifest and pure renderer

`packaging/release.json` contains no version or checksum literals. It defines:

- the binary name and archive contract already consumed by binstall;
- the current release targets and the target required for each manager;
- Homebrew formula fields and AUR package identity;
- nfpm package names, architecture mapping, license files, and distro package
  dependencies;
- Scoop and winget template identities, marked inactive until a verified
  `x86_64-pc-windows-msvc` release asset exists; and
- the upstream runtime dependency names used by `.deb`/`.rpm` metadata.

`packaging/release.py` uses only Python's standard library (`tomllib`, JSON,
hash/path validation, and deterministic text rendering). It has two modes:

- `validate`: read the workspace version, require `--tag v<that-version>`,
  validate every supplied archive/checksum/target mapping, and fail on an
  inactive target or an unknown placeholder;
- `render`: produce formula, PKGBUILD, nfpm specs, and Windows manifests under
  a caller-provided output directory. It never downloads, pushes, edits a
  release, or invokes a package manager.

The renderer must reject path traversal, missing checksums, checksum strings
that are not lowercase 64-hex SHA-256, version/tag mismatches, and attempts to
render Scoop/winget without the Windows asset. It should write files via a
temporary sibling and rename only after the whole output validates, so a
partial render cannot be mistaken for a complete package set.

The Homebrew output must retain the existing root-level `bin.install
"thegn"`, dual-license declaration, arm64-only constraint, and optional
runtime formula dependencies. The AUR output is `thegn-bin`: it consumes the
Linux musl archive, installs the root binary plus `tg` symlink and both license
files, and derives a legal Arch `pkgver` from the SemVer tag while retaining
the original tag in the source URL. The `.deb`/`.rpm` outputs consume the
Linux GNU archive and declare distro package dependencies; they do not vendor
those dependencies and do not claim to be hosted repositories.

`just release-package-dry-run` feeds a synthetic checksum map and fixture
archives to the renderer, checks every expected output, and proves the dry run
does not write outside its temporary directory. A separate release rehearsal
can pass real checksums from `just release-artifacts`; it must use a temporary
`XDG_STATE_HOME` for any `thegn` invocation.

## Release workflow

The tag workflow remains the single build authority:

1. `create-release` creates the existing draft.
2. The upload matrix builds the currently verified targets with the existing
   archive action. Each leg runs keyless `actions/attest-build-provenance`
   against the archive it built. The checksum remains a companion integrity
   file; no artifact-signing private key is introduced.
3. A `package` job waits for the complete upload matrix, downloads only this
   tag's release assets, invokes the renderer, invokes a pinned nfpm tool for
   the Linux GNU `.deb` and `.rpm`, and uploads all generated metadata/packages
   to the same draft. It fails before any external publish if an asset or
   checksum is missing.
4. The package job writes a summary listing every output and the external
   checklist state. The draft can still be reviewed and published manually as
   today.
5. A publisher step is conditional on an explicit repository/environment
   enablement flag and the two scoped credentials. When enabled, it clones
   the configured `homebrew-tap` repo and pushes `Formula/thegn.rb`, and
   pushes the rendered `PKGBUILD` plus generated `.SRCINFO` to the AUR
   `thegn-bin` repo. It must use a fresh branch/commit or an idempotent
   no-op, never rewrite release assets, and fail only its own job. When not
   enabled, it prints exactly which one-time setup item is missing and marks
   the publication as pending; it must not silently advertise the channel.

The exact trigger for external publication is either a reviewed manual
dispatch after the draft is published or a separate `release: published`
publisher workflow. The implementation must choose one and document it in
`RELEASING.md`; it must not push a tap formula pointing at an unpublished draft
as if that were a usable install. Re-running a publisher must be safe and must
not re-run `create-release`, whose destructive retry behavior is documented at
`.github/workflows/release.yml:45-67`.

Scoop and winget are generated templates, not active outputs on this branch.
They become active only in the follow-up that adds a verified Windows MSVC
archive to the release matrix. That follow-up also supplies the external
bucket/Windows-publishing account checklist. No Windows install command is
added to the README now.

## Batteries edition

`nix/batteries.nix` creates a package with one launcher, `thegn-batteries`:

- it runs the pinned `pkgs.alacritty` with `config/alacritty.toml`;
- it supplies the pinned `pkgs.nerd-fonts.fira-code` through a generated
  fontconfig file scoped to the launch;
- it launches the existing stable wrapped package, so the runtime tools remain
  the same closure already defined in `nix/package.nix:47-49,145-149`;
- on first launch it copies the immutable profile into
  `$XDG_CONFIG_HOME/thegn/alacritty.toml`, then exports
  `THEGN_ALACRITTY_CONFIG` to that copy; and
- it passes through user arguments without adding a daemon, state, or config
  migration.

The flake exposes `packages.<system>.batteries` for the already supported
systems and `nix run .#batteries` works through the package's `mainProgram`.
The default package and dev package remain unchanged. Home-manager's existing
`programs.thegn.package` option (`nix/hm-module.nix:239-247`) is sufficient:
the documented batteries example installs the batteries package in
`home.packages` and keeps the normal module for config ownership. No new HM
option is justified by this issue.

The bundled Alacritty profile already names FiraCode Nerd Font and sets
macOS Option-as-Alt and terminal identity (`config/alacritty.toml:4-11,21-25,
50-63`). This makes the Nix composition honest and testable. Ghostty remains a
shipped profile, but is not the pinned batteries emulator because the launcher
and font-picker plumbing are Alacritty-specific; the profile itself is
`config/ghostty.config:8-15,52-71`.

No macOS `install.sh --batteries` is added. The current installer performs a
source build and writes user launchers (`install.sh:106-124,174-224`); adding
brew/nix-darwin transactions would require real-host confirmation, rollback,
and platform rehearsals unavailable to this release workflow. No Homebrew cask
is added: an unsigned, locally generated `.app` cannot honestly be a
downloadable cask, and a formula cannot install a cask font as a portable
dependency. The documented entry criterion is a signed/notarized app and a
rehearsed cask policy. Linux distro provisioning, Windows Terminal profiles,
Flatpak, AppImage, and `nix bundle` are likewise deferred with criteria only.

## Files and sequencing

The three chunks below are file-disjoint. Chunk 2 consumes chunk 1's renderer
and templates, so the lead runs chunk 1, then chunk 2. Chunk 3 is independent
of both and may run in parallel with chunk 1; its `README.md` ownership is
exclusive to chunk 3.

## Verification and acceptance

- Pure generator tests cover version/tag matching, every active target,
  checksum substitution, stable-channel-only output, package names, AUR
  `pkgver`, nfpm specs, inactive Windows rejection, deterministic output, and
  failure isolation.
- `just release-package-dry-run` and shell/yaml/toml lint run without network
  credentials. `nix build .#thegn-nobridge` remains the routine Nix gate;
  `nix build .#batteries` is the batteries gate. No e2e is needed.
- A real release rehearsal records archive checksums, `gh attestation verify`,
  local `brew`/`makepkg` only when those external systems are configured, and
  `.deb`/`.rpm` installation in a disposable environment. The release notes
  and README advertise only paths with recorded evidence.
- Before handoff, run `openspec validate --all --strict` through the existing
  `just openspec-validate` recipe and the scoped checks specified in each
  chunk. Do not run `just test`, `just ci`, or e2e during this architecture
  task.
