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

   After downloading a draft release's `.deb`/`.rpm` assets with
   `gh release download <tag> --pattern 'thegn_*_amd64.deb' --pattern 'thegn-*-1.x86_64.rpm'`, run
   `just rehearse-packages deb=... rpm=...` (and optionally pass the previous release's packages as
   `previous_deb=... previous_rpm=...` for the upgrade leg). Attach the generated JSON evidence to
   THE-52. The standalone `.deb` / `.rpm` channel row changes only after a passing evidence record exists.

   ```sh
   just rehearse-packages deb=thegn_<version>_amd64.deb rpm=thegn-<version>-1.x86_64.rpm
   ```

6. **Tag + push.**

   ```sh
   git tag -a v<version> -m "thegn v<version>"
   git push origin v<version>
   ```

   The [`release`](.github/workflows/release.yml) workflow builds the `thegn`
   binary for **x86_64 linux (gnu + musl)** and **aarch64-apple-darwin** in
   read-only jobs. One upload broker receives the workflow artifacts, verifies
   the exact expected asset set and every SHA-256 checksum, rechecks the
   admitted tag, then publishes the archives and checksums to a **draft**
   GitHub Release. The broker also adds `thegn-<tag>-release.json`, recording
   the tag, tag object, commit, workflow SHA, run id, and sorted asset digests.
   Each archive receives keyless GitHub build-provenance attestation. The upload
   broker publishes the SHA-256 digests of every archive, checksum, and release
   record as a job output; the package job likewise publishes digests of the deb,
   rpm, and four rendered metadata assets it builds. These outputs bind consumers
   to bytes held by each producing job, independently of the mutable draft
   release. Before provenance verification or validation, the package job compares
   every downloaded archive and checksum against the upload broker's digests and
   refuses any mismatch. It then verifies every downloaded archive against the
   `release.yml` signer workflow and the signing run's `github.sha`,
   then requires each attestation's run invocation URL to identify that same
   workflow run. The upload job has already checked the tag against the admitted
   commit before creating the attestations. Downloaders can verify an archive
   with:

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

   windows-msvc is still out: that CI job has never executed. Add a target only
   once its CI job is green. Check the package job's
   summary for its exact outputs and target/checksum mapping. The
   `verify-manifest` job is the completeness proof: it checks the exact asset
   set, then requires every release asset to match the digest output of its
   producer. It also checks archive attestations, checksums, and the upload
   record, then attaches the deterministic `thegn-<tag>-manifest.json`. Write
   the release notes (crib from the changelog) and publish the draft only after
   `verify-manifest` is green and that manifest is attached.
   Pre-release tags (`-alpha.N` / `-beta.N`) are auto-marked as a prerelease
   by the workflow — no manual checkbox needed.

   > **Warning — retrying a failed run.** If a build leg fails, the broker does
   > not publish any archives. Use **"Re-run failed jobs" on the original
   > workflow run** so the leg and then the broker run against the same admitted
   > revision. Re-dispatching a build for a tag with an existing draft resumes
   > that draft without changing its notes or existing assets. A published
   > release makes `create-release` fail before uploads and is never deleted;
   > cut a new tag to rebuild a published version.

7. **Publish generated external metadata only after the draft is published.**
   In Actions, dispatch the `release` workflow from `main`, enter the already
   published tag, and choose `publish-external`. This operation is isolated
   from `create-release`, so it cannot change any release asset. Each
   publisher refuses to proceed unless the release manifest is present, its
   attestation verifies, it names the requested tag, and its formula or
   `PKGBUILD` entry matches the downloaded file's digest. Approve the
   protected `package-publication-homebrew` and `package-publication-aur`
   environments when prompted. Each has a required reviewer and only its
   matching deploy key.

   The credential-free `aur-srcinfo` job downloads `PKGBUILD` from the
   published release and runs `makepkg --printsrcinfo`, then passes `PKGBUILD`
   and `.SRCINFO` to the AUR publisher as an artifact. The Homebrew publisher
   pushes `Formula/thegn.rb` to the configured tap. The AUR publisher checks
   the artifact's `PKGBUILD` against the release asset before pushing both
   files to the AUR `thegn-bin` repository without interpreting the PKGBUILD.
   Each publisher records its cloned branch tip and uses a force-with-lease
   push against that observed tip, then verifies the remote branch with
   `git ls-remote`. The per-target job summary reports the committed old and
   new OIDs, an unchanged tip, or a failure and the observed remote state, so a
   Homebrew success and AUR failure are visible separately.
   Re-dispatching the same tag makes no commit when the repositories already
   contain identical output.

   Publication remains visibly pending until this one-time checklist is done:
   - Create a public repository named `homebrew-tap`, initially with a `main`
     branch, and create the AUR package repository named `thegn-bin`.
   - Add separate write-enabled deploy keys scoped only to the tap and AUR
     package repositories. They are not artifact-signing keys and have no write
     access to this source repository.
   - Create protected GitHub environments `package-publication-homebrew` and
     `package-publication-aur`, each with a required reviewer. Store only
     `HOMEBREW_TAP_DEPLOY_KEY` in the Homebrew environment and only
     `AUR_DEPLOY_KEY` in the AUR environment.
   - Set `HOMEBREW_TAP_REPOSITORY` to `<owner>/homebrew-tap` in the
     `package-publication-homebrew` environment, but leave
     `PACKAGE_PUBLICATION_ENABLED` unset.
   - Rehearse both generated outputs locally: install from a disposable local
     tap and run `makepkg --printsrcinfo` plus `makepkg` in a disposable AUR
     checkout.
   - Add the `aur.archlinux.org` host key to `.github/known_hosts` after
     checking it against the fingerprints published on the AUR home page with
     `ssh-keygen -lf`.
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

On the supported x86_64 Linux platform, the advertised install paths are Nix,
source installation, and the x86_64 Linux release archives. Nix and source
paths on other platforms remain best-effort where README says so. The release
workflow can render other outputs without making them supported channels:

| Channel                           | Current decision                                                                                                                 |
| --------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| Homebrew tap / AUR                | pending public repository setup and same-release clean-host rehearsal                                                            |
| standalone `.deb` / `.rpm` assets | pending clean-system install, upgrade, and uninstall rehearsal; no hosted apt/rpm repository                                     |
| Scoop / winget                    | deferred until THE-140's Windows artifact gate and THE-145's interactive parity evidence are green                               |
| hosted apt/rpm repositories       | deferred through the public-alpha cycle; their operational ownership is not justified while standalone assets remain unrehearsed |
| crates.io / `cargo binstall`      | deferred through the public-alpha cycle; source installs use this repository and no crate-publication contract is accepted       |

Enabling one of the deferred channels requires a separate bounded change with
an owner, credentials/operational model, and rehearsal evidence. It is not an
automatic consequence of producing another archive.

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

- **Homebrew — unaffected once published.** Homebrew does not attach
  `com.apple.quarantine` to formula downloads, so an unsigned binary installed
  with `brew` opens normally. Advertise this path only after the public tap
  rehearsal above is recorded.
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
