# Design — package-manager release automation

## The matrix, judged

Feasibility / recurring cost / automation for each candidate, and the
verdict. "Now" means this change; "staged" means spec'd entry criteria but
no work; "post-1.0" means deliberately not before.

| Manager                                       | Feasibility                                                         | Recurring cost once automated                                 | Verdict                                                                              |
| --------------------------------------------- | ------------------------------------------------------------------- | ------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| **Nix flake** (+ HM/darwin modules)           | shipped                                                             | zero (no release needed)                                      | done — canonical path                                                                |
| **GitHub release archives**                   | shipped                                                             | zero (tag-triggered)                                          | done — add provenance (now)                                                          |
| **Homebrew tap**                              | formula exists, tap missing                                         | ~zero: CI renders + pushes formula                            | **now**                                                                              |
| **AUR `thegn-bin`**                           | trivial: PKGBUILD over the musl asset                               | ~zero: CI pushes over SSH key                                 | **now**                                                                              |
| **mise / ubi**                                | works already by asset-name convention                              | zero — nothing to publish                                     | **now** (document only)                                                              |
| **cargo binstall (via `--git`)**              | works against the staged metadata + assets                          | zero                                                          | **now** (document; verify once)                                                      |
| **cargo install / plain binstall**            | blocked: `publish = false`, path deps across ~8 crates              | real: version discipline + publish step per crate per release | staged (post-alpha; RELEASING.md owns)                                               |
| **scoop / winget**                            | manifests are trivial JSON/YAML; blocked on a windows release asset | low (scoop autoupdates; wingetcreate PR per release)          | staged behind `add-windows-ci-distribution`'s msvc leg going green                   |
| **`.deb`/`.rpm` release assets (nfpm)**       | one config file over the musl binary                                | low, but no update path without a repo                        | recorded, not scoped — revisit on demand                                             |
| **hosted apt/rpm repos**                      | needs GPG key custody + repo hosting + distro matrix                | high, permanent                                               | post-1.0                                                                             |
| **homebrew-core / nixpkgs / distro official** | needs notability + community maintainers                            | outside our CI entirely                                       | post-1.0                                                                             |
| **flatpak / snap / AppImage**                 | wrong artifact for a TUI alone                                      | —                                                             | see `add-batteries-included-bundles` (THE-15) — only meaningful as a terminal bundle |

The dividing line is deliberate: **a manager makes the cut now iff release
CI can keep it correct with no per-release human step and no long-lived key
whose custody outlives the project's current size** (the AUR SSH key and
the tap deploy key are narrowly scoped write keys to two publishing
surfaces, not signing keys over the artifacts themselves).

## The artifact contract is the keystone

Everything downstream parses `thegn-<tag>-<target>.{tar.gz,zip}` +
`thegn-<tag>-<target>.sha256`:

- the staged `[package.metadata.binstall]` pkg-url
  (`crates/thegn-host/Cargo.toml`),
- the Homebrew formula URL,
- the AUR PKGBUILD `source=`,
- `ubi`/`mise`, which guess the right asset from the target triple in the
  name.

Today that naming is an implementation detail of `release.yml`; this change
promotes it to a spec'd contract so a rename is recognized as the breaking
change it is (it would silently strand every manifest at the old release).
The archive layout (binary at root, licenses + README beside it) is part of
the contract — the formula's `bin.install "thegn"` depends on it.

## Manifests job

A `manifests` job in `release.yml`, `needs: upload`, running once after the
whole matrix:

1. Download the release's `.sha256` assets via `gh` (same workflow, same
   tag — no cross-workflow trust).
2. Render `packaging/homebrew/thegn.rb` with version + darwin sha; commit
   to the `homebrew-tap` repo (`Formula/thegn.rb`) using a deploy key held
   as a secret. `packaging/homebrew/thegn.rb` stays the in-repo source of
   truth; the render substitutes the two placeholders only.
3. Render `packaging/aur/PKGBUILD` (musl asset) + regenerate `.SRCINFO`;
   push to `ssh://aur@aur.archlinux.org/thegn-bin.git` with the AUR key.
   `.SRCINFO` is generated in the job (container with `makepkg` or the
   well-worn publish action) so it can never disagree with the PKGBUILD.

Failure semantics: **fail-forward**. A manifest push failure fails its job
and leaves the release assets untouched; the job is idempotent and
re-runnable ("Re-run failed jobs" — matching the existing warning that
re-dispatching the whole workflow wipes the release). Manifests are pushed
only for non-draft-blocking errors to fix by rerun; the draft-release
publish step itself stays human (RELEASING.md), so the ordering is:
assets → maintainer publishes draft → manifests can also be dispatched
standalone if the tag's release is already published. Simplest correct
sequencing (manifests reference URLs that 404 until the draft is
published): the manifests job waits on release publish via the `release:
published` trigger rather than `needs:` — design detail for
implementation; either works, the spec only requires "rendered by CI from
the tag's published checksums, idempotent, never retracts assets".

## Provenance without keys

`actions/attest-build-provenance` on each upload leg (needs `id-token:
write` + `attestations: write` permissions). Keyless: signed via the
workflow's OIDC identity against the public sigstore/GitHub log, verified
with `gh attestation verify <file> -R blakeashleyjr/thegn`. This gives
users "this exact archive was built by release.yml at this tag" — which is
the attack sha256-beside-the-asset cannot address — at zero key custody.
Rejected alternatives:

- **minisign/cosign with a project key**: a long-lived private key in CI
  secrets is precisely the custody burden the macOS decision already
  declined; keyless attestation dominates it for this project size.
- **Notarization** (macOS): unchanged; RELEASING.md's decision and its
  revisit criteria stand. Homebrew stays the quarantine-free macOS path,
  which is exactly why the tap must actually exist.

## Channel tie-in

`add-release-channels` gives packaging a stable/dev split where dev is a
distinctly-named binary (`thegn-dev`). Package managers publish **stable
only**: one name, one artifact per target, and `THEGN_CHANNEL=dev` remains
the escape hatch on any binary. A `thegn-dev` AUR/brew presence would
double every manifest for an audience (channel-dev users) that is
explicitly nix/source-first. Spec'd as a requirement so a future "ship dev
to brew" is a conscious spec change.

## Verified-before-advertised

The README install matrix may only list a manager whose end-to-end install
was exercised (CI, or a recorded release rehearsal per RELEASING.md). This
is the same honesty rule release.yml already applies to targets ("add a
target here only once its CI job is green") extended to managers — it is
what keeps scoop/winget out until `add-windows-ci-distribution`'s msvc leg
exists, and what forces one real `brew install`/`makepkg -si` rehearsal
before the tap/AUR lines appear.

## Security

- **Credentials**: two new CI secrets — a deploy key scoped to push the
  `homebrew-tap` repo, and the AUR package SSH key. Neither can touch this
  repo, the release assets, or any user machine; blast radius of a leak is
  a malicious manifest push, which is (a) visible as a commit in a public
  repo, (b) defeated for the formula/PKGBUILD by their pinned sha256s only
  if the attacker cannot also repoint URLs — so treat both keys as
  compromising the respective install path and rotate on any workflow
  compromise. No raw tokens in config; secrets live only in GitHub Actions
  secrets (consistent with the SecretRef rule — nothing here touches thegn
  config at all).
- **Provenance**: keyless OIDC attestation, no long-lived signing key
  anywhere in the pipeline.
- **Supply chain**: builds stay `locked: true` against the committed
  Cargo.lock; manifests are rendered from checksums produced by the same
  tag's workflow, never hand-pasted.
- **No new runtime surface**: zero Rust changes; no capability-catalog row;
  the sandbox, permission model and control plane are untouched.

## Open questions

- Should `thegn doctor` learn "installed via X; vY available" (a
  best-effort, off-loop release check)? It is the natural home for AO 496's
  remainder, but it adds a network touch to doctor — deferred, not scoped.
- `.deb`/`.rpm` nfpm assets: cheap, but without a repo they have no update
  story. Add only if users ask.
- When the windows leg lands, does winget's portable-package type suffice
  or do users expect an installer? Decide inside the windows track.
