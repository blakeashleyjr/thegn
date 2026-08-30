# Releasing thegn

thegn versions follow SemVer with pre-release tags: `0.1.0-alpha.N` →
`0.1.0-beta.N` → `0.1.0`. Releases are tagged `v<version>` on `main`.

## Cutting a release

1. **Bump the version.** Edit `[workspace.package] version` in the root
   `Cargo.toml`, run `cargo build --workspace` to refresh `Cargo.lock`, and
   commit (`chore(release): v<version>`).
2. **Update the changelog.** Move the `## [Unreleased]` items under a new
   `## [<version>] — <date>` heading in `CHANGELOG.md`; add the compare/tag
   links at the bottom.
3. **Land on `main`** (via `thegn land` / the merge queue) and pull.
4. **Verify the full Nix install:** `just nix-build-full` (or dispatch `ci.yml`,
   which builds `.#default` on `workflow_dispatch`). The routine gate only
   builds `.#thegn-nobridge` — the shipped binary without the static-musl
   provider bridge — because building the bridge doubles the job. `nix profile
install github:blakeashleyjr/thegn` gives users `.#default`, bridge included,
   so it must be proven before a tag even though no release asset comes from it.
5. **Rehearse the build locally** — with remote CI paused, this is the only way
   to learn that a release build is broken _before_ the tag is public:

   ```sh
   just release-artifacts v<version>   # same archive + checksum shape as CI
   just release-verify   v<version>    # layout, runs, no quarantine
   ```

   The package renderer consumes the generated checksum; no release version or
   digest is pasted into a formula or package manifest. Note the archive is
   built for _this_ machine's target, so it rehearses the
   darwin leg on a Mac and the linux-gnu leg on Linux — not the musl one.

6. **Tag + push.**

   ```sh
   git tag -a v<version> -m "thegn v<version>"
   git push origin v<version>
   ```

   The [`release`](.github/workflows/release.yml) workflow builds the `thegn`
   binary for **x86_64 linux (gnu + musl)** and **aarch64-apple-darwin**, and
   attaches per-target archives + `.sha256` checksums (named
   `thegn-<tag>-<target>.sha256` — no `.tar.gz` infix) to a **draft** GitHub
   Release. Each archive also receives keyless GitHub build-provenance
   attestation; verify a downloaded archive with:

   ```sh
   gh attestation verify thegn-v<version>-<target>.tar.gz \
     --repo blakeashleyjr/thegn
   ```

   After the complete archive matrix succeeds, the package job downloads only
   that tag's draft assets, validates every active archive/checksum pair, and
   renders the Homebrew formula, AUR `PKGBUILD`, and nfpm specs from
   `packaging/release.json`. It then adds deterministic
   `thegn_<version>_amd64.deb` and `thegn-<version>-1.x86_64.rpm` assets plus
   the rendered metadata to the same draft. The `.deb` and `.rpm` are
   **standalone release assets**; there is no hosted apt or rpm update
   repository.

   windows-msvc is still out: that CI job has never executed, and
   `fail-fast: false` means an unbuilt target silently produces a partial asset
   set. Add a target only once its CI job is green. Check the package job's
   summary for its exact outputs and target/checksum mapping. Only after every
   archive, checksum, attestation, and generated package is present should the
   install channel be advertised. Write the release notes (crib from the
   changelog) and publish the draft.
   Pre-release tags (`-alpha.N` / `-beta.N`) are auto-marked as a prerelease
   by the workflow — no manual checkbox needed.

   > **Warning — retrying a failed run.** If some matrix legs fail, use
   > **"Re-run failed jobs" on the original workflow run**. Do NOT re-dispatch
   > the workflow for a tag whose release already exists: the create-release
   > step deletes the existing release (draft notes included — even a
   > _published_ release) and recreates it as an empty draft. Reserve
   > `workflow_dispatch` for full from-scratch rebuilds where losing the
   > release is acceptable.

7. **Publish generated external metadata only after the draft is published.**
   In Actions, dispatch the `release` workflow from `main`, enter the already
   published tag, and choose `publish-external`. This operation is isolated
   from `create-release`, so it cannot trigger the destructive release-recreate
   path described above or change any release asset. Approve the protected
   `package-publication` environment when prompted.

   The job downloads the generated formula and `PKGBUILD` from the published
   release. It pushes `Formula/thegn.rb` to the configured Homebrew tap, runs
   `makepkg --printsrcinfo`, and pushes `PKGBUILD` plus `.SRCINFO` to the AUR
   `thegn-bin` repository. Re-dispatching the same tag makes no commit when the
   repositories already contain identical output.

   Publication remains visibly pending until this one-time checklist is done:
   - Create a public repository named `homebrew-tap`, initially with a `main`
     branch, and create the AUR package repository named `thegn-bin`.
   - Add separate write-enabled deploy keys scoped only to the tap and AUR
     package repositories. They are not artifact-signing keys and have no write
     access to this source repository.
   - Create a protected GitHub environment named `package-publication`, add a
     required reviewer, and store the keys as `HOMEBREW_TAP_DEPLOY_KEY` and
     `AUR_DEPLOY_KEY` environment secrets.
   - Set repository variable `HOMEBREW_TAP_REPOSITORY` to
     `<owner>/homebrew-tap`, but leave `PACKAGE_PUBLICATION_ENABLED` unset.
   - Rehearse both generated outputs locally: install from a disposable local
     tap and run `makepkg --printsrcinfo` plus `makepkg` in a disposable AUR
     checkout.
   - Only after the rehearsal passes, set repository variable
     `PACKAGE_PUBLICATION_ENABLED=true` and dispatch `publish-external`.

   If any flag, repository, or credential is absent, the workflow emits an
   explicit warning containing this setup checklist and pushes nothing. It
   never reports Homebrew or AUR as live merely because package metadata was
   rendered.

   Modern Homebrew **refuses to install a formula from a file path** ("Homebrew
   requires formulae to be in a tap"), so there is no `brew install --formula
./thegn.rb` shortcut. To rehearse before enabling publication, make a local
   tap and copy the generated release asset into it:

   ```sh
   brew tap-new blakeashleyjr/tap                     # scaffolds Formula/
   cp thegn-v<version>-homebrew.rb \
     "$(brew --repository blakeashleyjr/tap)/Formula/thegn.rb"
   brew install blakeashleyjr/tap/thegn
   brew uninstall thegn && brew untap blakeashleyjr/tap   # when done
   ```

## Install paths this enables

- **Prebuilt binary** — download the linux-gnu, linux-musl, or
  aarch64-apple-darwin archive from the release page, verify the `.sha256`,
  extract `thegn` onto your `PATH`. On macOS see the Gatekeeper note below.
- **Nix** — `nix profile install github:blakeashleyjr/thegn` (works off any ref,
  no release needed). The flake has no private inputs, so this works for anyone;
  keep it that way — adding a private input silently breaks this path for
  everyone but the maintainer.
- **From source** — `./install.sh` (needs Rust/Cargo). On macOS it also generates
  the `thegn.app` launcher; `just macos-app` does the same for the other paths.
- **Homebrew / AUR** — only after step 7's publication job has pushed and the
  resulting repositories have been verified.

## macOS code signing and notarization — the decision

**thegn does not sign or notarize its releases, and the supported macOS install
paths are chosen so that it does not have to.** (Precisely: arm64 binaries are
_ad-hoc_ signed by the linker — `codesign -dv` reports `adhoc, linker-signed` —
because Apple silicon requires a signature to execute at all. That is not a
Developer ID signature and does not satisfy notarization; `spctl -a` rejects the
binary either way.) Notarization requires a paid
Apple Developer account ($99/yr), a Developer ID certificate held as a CI
secret, and a `notarytool` submit-and-staple step on every release. That is a
recurring cost and a key-management burden for a pre-alpha project with a
handful of users.

What that decision costs, precisely:

- **Homebrew — unaffected.** Homebrew does not attach `com.apple.quarantine` to
  formula downloads, so an unsigned binary installed with `brew` opens normally.
  This is the path to point macOS users at.
- **Nix — unaffected.** Same reason: the store path is not quarantined.
- **`./install.sh` / `just macos-app` — unaffected.** The `thegn.app` bundle is
  generated on the user's own machine, so it carries no quarantine attribute.
  This is _why_ the bundle is generated rather than shipped.
- **A tarball downloaded through a browser — affected.** It is quarantined, and
  the first launch **hangs on a Gatekeeper dialog** rather than failing fast
  (verified on macOS 15: the process sits there until the dialog is answered).
  Clearing the attribute first avoids it:

  ```sh
  xattr -dr com.apple.quarantine ./thegn
  ```

  Do that **before** the first run. Once macOS has denied a binary, the verdict
  is cached by `syspolicyd` and removing the attribute afterwards may not be
  enough — the binary keeps stalling. Recovering then means approving it under
  System Settings → Privacy & Security, or re-extracting to a fresh path.

Revisit this when either becomes true: a macOS `.app` or `.pkg` is distributed
directly (a Homebrew _Cask_ would need it), or enough users are hitting the
quarantine prompt that the support cost exceeds the certificate's.

## Deferred release channels

- **Scoop and winget** require a green Windows MSVC release archive before
  their generated manifests can be enabled.
- **Hosted apt/rpm repositories** require an owner for hosting, repository
  metadata, and signing-key custody. The current `.deb` and `.rpm` assets do
  not provide automatic updates.
- **crates.io / plain `cargo install` / `cargo binstall`** require a deliberate
  workspace publication decision.

### crates.io detail

`crates/thegn-host` is `publish = false` and the workspace uses path
dependencies, so the crates cannot be published to crates.io as-is. Enabling
`cargo install thegn` / `cargo binstall thegn` is a post-alpha task: it requires
publishing the workspace's library crates (thegn-core, thegn-svc, the gtui-\*
family, tg-kit) with real version requirements, then flipping thegn-host to
`publish = true`. The `[package.metadata.binstall]` block in
`crates/thegn-host/Cargo.toml` is already staged to point at the release assets
for when that happens.
