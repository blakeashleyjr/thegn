# Automate package-manager releases from the release pipeline

Linear: THE-52

## Why

THE-52 asks for "releases to every possible package manager". The honest
reading for a pre-1.0 project with a paused remote-CI budget is not "every
possible" but **every manager whose recurring cost is near zero once
automated, staged behind the gates that already exist**. Today the release
story is:

- **Nix** (canonical): `nix profile install github:blakeashleyjr/thegn`,
  plus home-manager/nix-darwin modules and stable/dev channel outputs.
  Reproducible, no release needed. Done.
- **GitHub Releases** (`release.yml`): linux gnu+musl + aarch64-darwin
  archives with sha256 checksums, drafted on tag push. Done, but **unsigned
  and unattested** — a checksum published next to the artifact it checks
  proves transport integrity, not provenance.
- **Homebrew**: a complete formula sits at `packaging/homebrew/thegn.rb`,
  but **the tap repo does not exist** (RELEASING.md step 7 is a manual
  copy-and-paste ritual into a repo nobody has created). macOS's blessed
  path — the one that dodges Gatekeeper quarantine — is the one users
  cannot actually run.
- **cargo install / binstall**: blocked on publishing the workspace's
  library crates (`publish = false`, path deps); the
  `[package.metadata.binstall]` block is staged and inert (RELEASING.md
  "Not yet").
- **AUR, apt/rpm, scoop/winget, mise**: nothing.

Every manual step in that list is a step that gets skipped under release
pressure — the formula still says `REPLACE_WITH_AARCH64_SHA256`. The fix is
the same shape as the rest of the repo's automation: the release workflow
that builds the artifacts also renders and publishes the manifests that
point at them, so a manifest can never drift from its assets.

## What Changes

- **Provenance for every artifact.** `release.yml` gains a
  `actions/attest-build-provenance` step (keyless, GitHub OIDC — no
  long-lived signing key to manage) attaching SLSA build provenance to each
  uploaded archive, verifiable with `gh attestation verify`. Checksums stay.
  The macOS no-notarization decision (RELEASING.md) is reaffirmed, not
  changed.
- **Homebrew tap, automated.** The tap repo (`homebrew-tap`) is created
  once; `packaging/homebrew/thegn.rb` becomes the render template; a
  post-matrix `manifests` job substitutes the tag's version + the published
  `.sha256` and pushes `Formula/thegn.rb` to the tap with a write-scoped
  deploy key. RELEASING.md step 7 shrinks to "verify the bump commit".
- **AUR `thegn-bin`, automated.** A `packaging/aur/` PKGBUILD template
  (prebuilt musl artifact — not a source package; a full cargo build is the
  wrong ask of AUR builders) plus generated `.SRCINFO`, published by the
  same `manifests` job over the AUR package's SSH key.
- **Zero-maintenance conventions, documented.** The archive naming
  (`thegn-<tag>-<target>.tar.gz`) is promoted to a spec'd public contract:
  it is what the staged binstall pkg-url, the formula, the PKGBUILD **and**
  convention-based installers (`mise`/`ubi`) all parse. README gains an
  install matrix including `ubi:`/`mise` one-liners and the
  `cargo binstall --git` interim path — install methods that cost nothing
  per release because they resolve assets by convention.
- **Deferred managers get entry criteria, not hand-waving.** crates.io
  (`cargo install`/plain `binstall`) stays post-alpha behind the workspace
  publish decision; **scoop/winget stay behind the windows-msvc release leg
  going green** (`add-windows-ci-distribution` — that CI job has never
  executed); hosted apt/rpm repos and homebrew-core/nixpkgs-upstream are
  post-1.0 (they add GPG key custody / community-maintainer processes with
  no payoff at current user counts). Standalone `.deb`/`.rpm` release
  assets via nfpm are recorded as a cheap later option, not scoped.
- **Channel rule.** Everything published to a package manager is the
  **stable** channel; the dev channel remains nix (`.#dev`) / source, with
  `THEGN_CHANNEL` as the runtime override either way (aligns with
  `add-release-channels`).

## Non-goals

- Publishing the workspace to crates.io (post-alpha; RELEASING.md owns it).
- scoop/winget manifests now (staged; unblocers listed in the spec).
- Hosted apt/rpm repositories, homebrew-core, nixpkgs upstream (post-1.0).
- macOS code signing / notarization (decided in RELEASING.md; unchanged).
- Any in-binary self-update mechanism (AO 496 stays with the package
  managers; a `doctor` version-check is an open question in design.md).

## Impact

- Roadmap: **AO 494** (single-command install), **AO 496** (update/upgrade
  mechanism — package managers are the update path), **A 5** (single-binary
  distribution).
- Specs: new capability **`distribution`** (5 ADDED requirements: artifact
  contract, provenance, CI-rendered manifests, stable-channel-only
  publishing, verified-before-advertised).
- Code: `.github/workflows/release.yml` (attest + manifests job),
  `packaging/homebrew/` (templateize), `packaging/aur/` (new), README /
  RELEASING.md / `docs/help/` install prose. **No Rust changes, no
  capability-catalog row** — packaging is out-of-process; nothing here is a
  door into a running instance (same precedent as
  `package-shell-completions`).
- In-flight reconciliation: `add-release-channels` (stable/dev packaging
  split — this change consumes it, publishes stable only);
  `add-windows-ci-distribution` (owns the windows artifact; scoop/winget
  are staged behind it and scoped here only as entry criteria);
  `package-shell-completions` (nix-side completions; tarball users keep the
  documented one-liners — no completions inside release archives, per that
  change's non-goal); `add-batteries-included-bundles` (THE-15, sibling
  change: bundled-terminal editions build ON these artifacts and this
  capability).
