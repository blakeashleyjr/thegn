# Design — verified package-channel publication

## Machine-readable release contract

`packaging/release.json` is the single input to the deterministic renderer. It
defines archive naming/layout, enabled targets, runtime dependencies, package
templates, and explicitly disabled future managers. Renderer tests reject
missing, extra, mismatched, or unsafe outputs so workflow and templates cannot
silently drift.

The current supported build matrix is Linux GNU/musl and aarch64 Darwin.
Homebrew metadata targets Darwin, AUR targets musl, and nfpm deb/rpm packages
target Linux GNU. Windows/Scoop/winget remain disabled until their artifact leg
is enabled and proven.

## Build and provenance pipeline

Tagged builds produce the stable archive/checksum contract. A downstream
package job downloads and validates those exact inputs, renders Homebrew/AUR/
nfpm metadata, builds deb/rpm packages, and uploads the rendered metadata and
packages as release assets. GitHub OIDC attestations provide keyless provenance
for built artifacts; no long-lived artifact-signing key is introduced.

## Protected publication

Publishing Homebrew and AUR metadata is a separate protected/manual job. It
validates that the public repositories and narrowly scoped deploy credentials
are configured, handles key files outside argv/log output, regenerates AUR
`.SRCINFO`, and commits/pushes only when the rendered content changes. Failure
does not retract release assets. Re-running the same tag must be idempotent.

## Verified-before-advertised rule

A channel is public/supported only after a recorded real release rehearsal
publishes its metadata, installs on a clean representative system, verifies the
binary version/checksum/attestation, and exercises relevant upgrade/uninstall
behavior. Homebrew/AUR external setup and these rehearsals are still open.
Deb/rpm assets likewise require installation evidence.

Hosted apt/rpm repos, crates.io, and Windows managers each carry ongoing cost or
an unmet artifact dependency. They remain decisions with explicit entry
criteria, not implied future scope. Package managers publish stable only; dev
remains Nix/source/runtime override.
