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

Each selected release asset remains subject to the 512 MiB archive cap and
64 KiB text-asset cap. The aggregate selected-download cap is 1 GiB, twice the
archive cap. The three enabled target archives in v0.1.0-alpha.103 were
46,161,052, 55,302,039, and 55,513,949 bytes (156,977,040 bytes total); all
listed release assets in that candidate totaled 156,981,077 bytes. This leaves
about 6.8x headroom over the observed target archives while bounding the
unfiltered manifest download. The aggregate cap was previously an owner-number
question; 1 GiB is the conservative implementation default pending owner
confirmation of the operational budget.

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

After both publishers finish, a credential-free, read-only job downloads their
receipts by immutable artifact ID and writes one saga ledger. The artifact IDs
preserve the successful publisher output when GitHub reruns only a failed job;
the ledger records each receipt's original publication attempt. A missing
receipt after a ready publisher is unknown remote state, not proof of no push or
unchanged content. Partial and unknown states are surfaced in the workflow
summary and require an operator to inspect both target tips and reconcile.
Per-target receipts and the combined ledger have 90-day artifact retention;
longer-term retention needs a separate archival policy.

Draft producer uploads preserve the public asset names but never replace an
existing asset. A shared planner compares GitHub's per-asset SHA-256 metadata
with local bytes, uploads only missing names, and refuses conflicts or unusable
metadata. The release record binds package and manifest uploads to the admitted
tag, source, trusted workflow revision, and workflow run ID. Failed-job retries
within the same run can reuse exact assets; a new workflow run cannot take over
a populated draft whose generation record belongs to another run. A same-name
race fails closed because the final `gh release upload` has no replacement
option. The release workflow uses one tag-only concurrency group for builds,
release publication, and external metadata publication. A protected
`publish-release` operation verifies the downloaded exact asset set, signed
manifest, successful producer run, and live tag identity before transitioning
the draft. A verified replay of an already-published release is read-only.
GitHub's UI publication cannot participate in workflow concurrency; it is
documented as break-glass only after all runs for that tag are stopped, with no
later producer retry against that draft. Break-glass release retraction remains
separate work.

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
