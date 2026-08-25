# Tasks

## 1. Provenance

- [ ] 1.1 `release.yml`: add `id-token: write` + `attestations: write`
      permissions and an `actions/attest-build-provenance` step per upload
      leg covering the archive (and checksum) it just published.
- [ ] 1.2 RELEASING.md + README: document
      `gh attestation verify <archive> -R <owner>/thegn` beside the
      existing sha256 instructions.

## 2. Homebrew tap

- [ ] 2.1 Create the public `homebrew-tap` repo (one-time, manual) and add
      a write-scoped deploy key; store the private half as a release-repo
      Actions secret.
- [ ] 2.2 Turn `packaging/homebrew/thegn.rb` into the render template
      (explicit `@VERSION@` / `@SHA256_ARM64_DARWIN@` placeholders replace
      the current hand-edit fields); keep it the in-repo source of truth.
- [ ] 2.3 Add the `manifests` job: fetch the tag's published `.sha256`
      assets via `gh`, render the formula, commit to
      `homebrew-tap/Formula/thegn.rb`. Idempotent; fails without touching
      release assets. Decide trigger sequencing vs the draft-publish step
      (release `published` event or a dispatch input) per design.md.
- [ ] 2.4 Rehearse once end-to-end (local tap per RELEASING.md, then the
      real tap on the next release) and record it; only then add the
      `brew install` line to the README matrix. Shrink RELEASING.md step 7
      to "verify the bump commit".

## 3. AUR

- [ ] 3.1 `packaging/aur/PKGBUILD.template` for `thegn-bin`: linux-musl
      release asset, installs `thegn` + `tg` symlink + licenses; conflicts
      with a future source `thegn` package.
- [ ] 3.2 One-time: create the AUR `thegn-bin` package base; store its SSH
      key as an Actions secret.
- [ ] 3.3 Extend the `manifests` job: render PKGBUILD, generate `.SRCINFO`
      in the job (never hand-edited), push to AUR. Same idempotence and
      failure isolation as 2.3.
- [ ] 3.4 Rehearse `makepkg -si` against a real release before the AUR
      line enters the README matrix.

## 4. Conventions + docs

- [ ] 4.1 Verify `ubi`/`mise` resolve the current release assets by
      convention (one manual run against the latest tag); document the
      one-liners.
- [ ] 4.2 Verify `cargo binstall --git` resolves the staged
      `[package.metadata.binstall]` against a real release; document as the
      interim path until crates.io publishing.
- [ ] 4.3 README install matrix: nix, prebuilt archive (+ verify), brew
      tap, AUR, mise/ubi, binstall-from-git; deferred managers listed with
      their entry criteria, not instructions. Mirror the essentials in
      `docs/help/` install prose if the help corpus carries an install
      page (no new action ids — help ratchets unaffected).
- [ ] 4.4 RELEASING.md: fold the new automation into the release steps;
      note the two publishing credentials and their scope/rotation story.

## 5. Validation

- [ ] 5.1 Run `just ci` once, when the implementation is complete
      (includes `openspec validate --all --strict`; the workflow changes
      themselves are proven by the next tagged release / a dispatch
      rehearsal, since remote CI is dispatch-only).
