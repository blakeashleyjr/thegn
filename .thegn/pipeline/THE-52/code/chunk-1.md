# Chunk 1 — deterministic release manifest and renderers

## Scope

Create the pure packaging seam. The renderer is an offline library/CLI in a
small Python module, not Rust and not `thegn-core`. Cargo's workspace version
remains authoritative; `packaging/release.json` supplies the artifact,
manager, and dependency catalog. Templates contain no release-specific
version, tag, checksum, or secret.

## Exact files touched

- `packaging/release.json` (new)
- `packaging/release.py` (new)
- `packaging/tests/test_release.py` (new)
- `packaging/homebrew/thegn.rb` → `packaging/homebrew/thegn.rb.tmpl` (rename
  the existing formula into a template)
- `packaging/aur/PKGBUILD.tmpl` (new)
- `packaging/nfpm.yaml.tmpl` (new)
- `packaging/scoop/thegn.json.tmpl` (new, inactive until Windows is enabled)
- `packaging/winget/thegn.yaml.tmpl` (new, inactive until Windows is enabled)
- `justfile` (add only the packaging dry-run/rehearsal recipes)

No other chunk may edit these paths. Do not edit Cargo manifests, config
examples, help ratchets, or the release workflow in this chunk.

## Approach

1. Parse `Cargo.toml` with `tomllib` and read
   `workspace.package.version`. Require `--tag` to equal `v<version>`; reject
   an alpha/beta mismatch rather than silently rendering a package for a
   different commit.
2. Define the existing archive contract in `release.json`: Unix `.tar.gz`
   versus Windows `.zip`, root-level `thegn`, both license files and README,
   current green targets (`x86_64-unknown-linux-gnu`,
   `x86_64-unknown-linux-musl`, `aarch64-apple-darwin`), and future
   `x86_64-pc-windows-msvc` as disabled. Define the Homebrew/darwin, AUR/musl,
   nfpm/GNU, Scoop/MSVC, and winget/MSVC target mappings in one place.
3. Keep the existing binstall block as the compatibility contract. The test
   reads `crates/thegn-host/Cargo.toml` and fails if its URL/format no longer
   matches the manifest's archive contract. Do not enable crates.io publishing:
   all workspace packages are currently `publish = false`.
4. Render the Homebrew template with the current arm64 checksum, preserving
   the existing formula's root install, license, optional runtime formula
   dependencies, and stable binary name. Render `thegn-bin` from the musl
   checksum; normalize the SemVer prerelease only for Arch `pkgver`, while the
   source URL retains the original release tag.
5. Render a parameterized nfpm spec for the GNU archive. It must install the
   root binary and `tg` symlink, carry both licenses, map the target to the
   distro architecture, and declare (not bundle) the manifest's runtime
   dependencies. Produce standalone `.deb` and `.rpm` artifacts later in CI;
   this chunk only renders the deterministic spec.
6. Render Scoop/winget only when the manifest says the MSVC asset is enabled
   and a checksum is supplied. Otherwise return a clear “Windows release lane
   is not enabled” error; never emit a plausible but broken manifest.
7. Validate placeholders, checksum shape (lowercase 64-hex SHA-256), target
   names, output paths, and required files before atomically replacing the
   temporary output directory. Never fetch, push, call a package manager, or
   read a secret.
8. Add `just release-package-dry-run tag=...` using fixture checksum/archive
   inputs and `just release-package-validate ...` for a real release asset
   directory. Both use temporary directories and never invoke `thegn` without
   an isolated `XDG_STATE_HOME`.

## Tests to run

- `python3 -m unittest discover -s packaging/tests -p 'test_*.py'`
- `just release-package-dry-run tag=v0.1.0-alpha.2`
- `just quick thegn-host` (scoped repository sanity check; no Rust source is
  added)
- `cargo nextest run -p thegn-host --lib` (scoped host regression check; use
  `XDG_STATE_HOME` under a temporary directory if any test invokes the binary)
- `shellcheck -x packaging/release.py` is not applicable to Python; run the
  repository's normal `yamllint`/`taplo` checks over the new manifest/templates
  and `python3 -m py_compile packaging/release.py` instead.

Do not run `just test`, `just ci`, a full workspace compile, or e2e.

## Dependencies / overlap

Chunk 1 is the only owner of the renderer, release manifest, package
templates, and packaging `just` recipes. Chunk 2 depends on its CLI contract
and must run after this chunk; it owns different files. Chunk 3 is independent
and owns different files. There is no file overlap.

## Done criteria

- A clean checkout can render every currently active manager output from a
  synthetic checksum map with no network, credentials, or binary execution.
- The output is byte-for-byte deterministic and contains no `REPLACE_WITH_*`,
  unreplaced placeholder, `thegn-dev`, or hand-edited `.SRCINFO`.
- The tests prove the current binstall metadata and archive layout contract,
  reject missing/mismatched checksums and tags, reject inactive Windows, and
  prove failed rendering leaves no partial output.
- No config key, action id, capability row, completion surface, provider seam,
  or `thegn-core` code is added; all architecture ratchets remain unchanged.
- Commit the chunk with exactly:

  `feat(packaging): add deterministic release manifest renderer`
