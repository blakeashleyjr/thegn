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
5. **Tag + push.**

   ```sh
   git tag -a v<version> -m "thegn v<version>"
   git push origin v<version>
   ```

   The [`release`](.github/workflows/release.yml) workflow builds the `thegn`
   binary for **x86_64 linux (gnu + musl)** and **aarch64-apple-darwin**, and
   attaches per-target archives + `.sha256` checksums (named
   `thegn-<tag>-<target>.sha256` — no `.tar.gz` infix) to a **draft** GitHub
   Release. windows-msvc is still out: that CI job has never executed, and
   `fail-fast: false` means an unbuilt target silently produces a partial asset
   set. Add a target only once its CI job is green. Write the release notes
   (crib from the changelog) and publish.
   Pre-release tags (`-alpha.N` / `-beta.N`) are auto-marked as a prerelease
   by the workflow — no manual checkbox needed.

   > **Warning — retrying a failed run.** If some matrix legs fail, use
   > **"Re-run failed jobs" on the original workflow run**. Do NOT re-dispatch
   > the workflow for a tag whose release already exists: the create-release
   > step deletes the existing release (draft notes included — even a
   > _published_ release) and recreates it as an empty draft. Reserve
   > `workflow_dispatch` for full from-scratch rebuilds where losing the
   > release is acceptable.

6. **Bump the Homebrew formula** (`packaging/homebrew/thegn.rb`): set `version`
   and paste the `sha256` from the release's
   `thegn-<tag>-aarch64-apple-darwin.sha256` asset. The formula is Apple-silicon
   only, matching the matrix.

   The tap repo does not exist yet. Until it does, users can install straight
   from the file — `brew install --formula ./packaging/homebrew/thegn.rb`. To
   create it: a public repo named exactly `homebrew-tap` under the same owner,
   with the formula at `Formula/thegn.rb`; `brew install blakeashleyjr/tap/thegn`
   then resolves it. Keep this file as the source of truth and copy it over on
   each release.

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
- **Homebrew** — once the tap exists (step 6).

## macOS code signing and notarization — the decision

**thegn does not sign or notarize its releases, and the supported macOS install
paths are chosen so that it does not have to.** Notarization requires a paid
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
  the first launch is refused until the user clears it:

  ```sh
  xattr -dr com.apple.quarantine ./thegn
  ```

Revisit this when either becomes true: a macOS `.app` or `.pkg` is distributed
directly (a Homebrew _Cask_ would need it), or enough users are hitting the
quarantine prompt that the support cost exceeds the certificate's.

## Not yet: crates.io / `cargo binstall`

`crates/thegn-host` is `publish = false` and the workspace uses path
dependencies, so the crates cannot be published to crates.io as-is. Enabling
`cargo install thegn` / `cargo binstall thegn` is a post-alpha task: it requires
publishing the workspace's library crates (thegn-core, thegn-svc, the gtui-\*
family, tg-kit) with real version requirements, then flipping thegn-host to
`publish = true`. The `[package.metadata.binstall]` block in
`crates/thegn-host/Cargo.toml` is already staged to point at the release assets
for when that happens.
