# Chunk 2 — release workflow, provenance, and publication checklist

## Scope

Connect the existing tag build to Chunk 1. Every tag produces validated
package inputs and standalone `.deb`/`.rpm` release assets. Homebrew/AUR
publication is explicit and isolated because it requires external repositories
and scoped credentials. The current draft-release flow and its safe retry rule
must remain intact.

## Exact files touched

- `.github/workflows/release.yml`
- `RELEASING.md`

No other chunk may edit these paths. Chunk 1 owns all `packaging/**` paths;
Chunk 3 owns `README.md`, Nix files, and help pages.

## Approach

1. Add job-level permissions for `id-token: write` and `attestations: write`
   only where needed, retaining `contents: write` for the existing release.
   After each `upload` leg creates its archive, run
   `actions/attest-build-provenance` against that exact local archive. Do not
   add a long-lived artifact-signing key; checksums remain release assets.
2. Add a `package` job after the complete upload matrix. Resolve the tag in one
   shell step, download only that tag's assets from the draft using
   `GITHUB_TOKEN`, and pass their checksums to `packaging/release.py validate`
   and `render`. If any currently active target/checksum is absent, stop before
   any publication or asset mutation.
3. Run nfpm from a pinned, reproducible source available to CI (prefer the
   repository's pinned Nix tool input or a pinned nfpm release with checksum;
   do not use an unpinned `latest`). Extract the Linux GNU archive into a
   scratch directory, render the spec, and create the `.deb` and `.rpm` with
   deterministic filenames. Upload the packages and rendered metadata to the
   same draft with `--clobber` semantics matching the existing completion job.
4. Add a visible job summary containing output names, target/checksum mapping,
   provenance verification command, and whether external publication is
   enabled. A renderer or package failure fails this job; it never deletes,
   retracts, or overwrites the source release archives.
5. Choose and implement one safe external-publication trigger, documented in
   `RELEASING.md`: either a reviewed manual dispatch after the draft is
   published, or a separate release-published event path. It must not rerun
   `create-release`, because that job deletes an existing release. The
   publisher clones the configured Homebrew tap and pushes `Formula/thegn.rb`,
   and pushes rendered `PKGBUILD` plus `makepkg --printsrcinfo` output to the
   AUR `thegn-bin` repo. Re-running the same tag is a no-op or identical
   commit.
6. Gate publication behind an explicit environment/repository enablement flag.
   The workflow must check each required secret and emit an `::warning::` plus
   the exact setup checklist when absent; it must not silently claim that brew
   or AUR is live. Credentials are narrowly scoped to the tap repo and AUR
   package repo, never artifact signing or this source repo.
7. Update `RELEASING.md` to replace the manual formula paste ritual with the
   generated-output flow, add `gh attestation verify`, explain `.deb`/`.rpm`
   are standalone assets with no hosted update repository, and record the
   one-time checklist: create public `homebrew-tap`, create AUR
   `thegn-bin`, add write-scoped deploy keys, configure the GitHub environment,
   perform a local tap/makepkg rehearsal, then enable publication. Do not add
   README install claims here; Chunk 3 adds only verified claims.
8. Record deferred external work without pretending it is active: Scoop and
   winget require a green Windows MSVC release archive first; crates.io/plain
   `cargo install` requires a deliberate workspace publish decision; hosted
   apt/rpm repositories require signing-key/hosting ownership.

## Tests to run

- `python3 -m unittest discover -s packaging/tests -p 'test_*.py'` (renderer
  contract used by the workflow)
- `just release-package-dry-run tag=v0.1.0-alpha.2`
- `just quick thegn-host` (scoped build sanity; release workflow is otherwise
  out-of-process)
- `cargo nextest run -p thegn-host --lib` (scoped host regression check, with
  temporary `XDG_STATE_HOME` for any binary invocation)
- `yamllint .github/workflows/release.yml`
- `git diff --check`

For a real rehearsal, use a disposable local GitHub/tap checkout and a
temporary release asset directory; never run a downloaded worktree binary
against the live state DB. Do not run `just ci`, full tests, or e2e in this
chunk.

## Dependencies / overlap

Chunk 2 is serial after Chunk 1 because the workflow invokes its renderer and
templates, but the files are disjoint. Chunk 3 is independent and may run in
parallel. No coder may “fix” a packaging template in this chunk; report the
renderer defect back to Chunk 1 instead.

## Done criteria

- A tag run still creates the same draft and current archive/checksum set, now
  with an attestation per archive and a post-matrix package job.
- The package job emits deterministic Homebrew/AUR metadata, nfpm specs, and
  valid standalone `.deb`/`.rpm` assets from the Cargo version plus release
  manifest; no version/checksum is pasted in YAML or `RELEASING.md`.
- Missing external accounts produce an explicit, actionable pending checklist;
  they do not fail or silently pass as a published channel. When enabled,
  publication is scoped, idempotent, and cannot touch release assets.
- `RELEASING.md` describes the exact safe retry/dispatch order and the
  verified-before-advertised rule. Scoop/winget and crates.io remain deferred.
- Commit the chunk with exactly:

  `ci(release): render and attest package artifacts`
