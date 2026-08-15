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
4. **Tag + push.**
   ```sh
   git tag -a v<version> -m "thegn v<version>"
   git push origin v<version>
   ```
   The [`release`](.github/workflows/release.yml) workflow builds the `thegn`
   binary for linux (gnu + musl), macOS (arm + x86-64), and windows-msvc, and
   attaches per-target archives + `.sha256` checksums to a **draft** GitHub
   Release. Write the release notes (crib from the changelog) and publish.
5. **Bump the Homebrew formula** (`packaging/homebrew/thegn.rb`): update
   `version` and paste the two macOS `sha256` values from the release's
   `*-apple-darwin.tar.gz.sha256` assets. Commit to the tap.

## Install paths this enables

- **Prebuilt binary** — download the archive for your platform from the release
  page, extract `thegn` onto your `PATH`.
- **Homebrew** — `brew install <owner>/tap/thegn` (once the tap carries the
  bumped formula).
- **Nix** — `nix profile install github:blakeashleyjr/thegn` (works off any ref,
  no release needed).
- **From source** — `./install.sh` (needs Rust/Cargo).

## Not yet: crates.io / `cargo binstall`

`crates/thegn-host` is `publish = false` and the workspace uses path
dependencies, so the crates cannot be published to crates.io as-is. Enabling
`cargo install thegn` / `cargo binstall thegn` is a post-alpha task: it requires
publishing the workspace's library crates (thegn-core, thegn-svc, the gtui-\*
family, tg-kit) with real version requirements, then flipping thegn-host to
`publish = true`. The `[package.metadata.binstall]` block in
`crates/thegn-host/Cargo.toml` is already staged to point at the release assets
for when that happens.
