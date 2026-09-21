# Pinning GitHub Actions

Every external action under `.github/` is pinned to a **full 40-character
commit SHA**, with the human-readable version in a trailing comment:

```yaml
- uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0
```

`test/action_pin_test.py` enforces this (`just test-action-pins`, and part of
`just test`).

## Why

GitHub tags are **not immutable** — a tag can be force-moved to any commit at
any time, by anyone who can push to that repository. `@main` and `@stable` are
branches, so they move by design.

That matters here because of what these workflows hold. `release.yml` grants
`contents: write`, `id-token: write` and `attestations: write`, and passes
publishing secrets; `sandbox-image.yml` holds `packages: write` and pushes
registry images; the CI Cachix job carries `CACHIX_AUTH_TOKEN` and
`NIX_GITHUB_TOKEN`. An action resolved through a mutable ref in any of those
jobs is an open invitation: whoever can retarget the ref — or anyone who
compromises that upstream repository — executes arbitrary code with those
permissions and those secrets. A SHA cannot be retargeted; the content is the
name.

## The scan is recursive, and that is the point

A workflow step that calls a **local composite action** inherits everything
that action invokes. Pinning only the workflow layer therefore proves nothing,
and this repository was a live example of the failure: every caller of
`./.github/actions/ci-setup` looked clean while the composite itself ran
`DeterminateSystems/nix-installer-action@main` — a mutable branch — in a job
that receives a GitHub token.

So `action_pin_test.py` follows `./`-relative `uses:` references into their
`action.yml` and checks those too. It also scans composite actions that no
workflow currently calls, so an unreferenced action cannot quietly rot.

## Refreshing a pin

Resolve the tag you want to the commit it points at, then update both the SHA
and the comment:

```sh
gh api repos/actions/checkout/commits/v4 --jq .sha
```

Review what changed between the old and new SHA before taking it — that review
_is_ the security control. A pin you bump without reading is a mutable ref with
extra steps.

Two cases need more than a substitution:

- **`dtolnay/rust-toolchain`** infers the toolchain from the ref _name_
  (`@stable`). Pinned by SHA that inference is gone, so the call sites pass
  `toolchain: stable` explicitly. Keep that input when bumping.
- **`DeterminateSystems/nix-installer-action`** was called as `@main` in
  `ci-setup` and `@v16` elsewhere. Both now point at the v16 commit; prefer a
  released tag's SHA over a branch head, so the pin corresponds to something
  upstream actually published.

## Exceptions

`test/action-pin-allowlist.txt` holds reviewed exceptions, one `owner/repo@ref`
per line. It is **seeded empty** and should stay that way. An entry means a
privileged workflow deliberately runs a mutable reference: record who reviewed
it and why a pin is not possible, and delete it as soon as it is.
