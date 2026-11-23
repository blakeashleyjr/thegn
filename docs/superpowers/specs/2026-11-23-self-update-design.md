# Built-in update checks and self-update — design

**Date:** 2026-11-23 · **Status:** approved design, not yet planned or implemented

## Goal

thegn tells the user when a newer release exists and, where it owns its own
binary, installs that release itself after verifying it. Background
auto-update is available as an opt-in. Today thegn never checks for new
versions (`docs/help/privacy.md`), and users of the release archive — including
WSL2 testers — update by hand.

## Decisions

| Question                    | Decision                                                                                                                                   |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| Scope                       | Check, notify, and self-replace where safe. Full background auto-update is an opt-in setting.                                              |
| Check default               | On by default, easy to disable. Never runs in CI, tests or sandboxed panes.                                                                |
| When an update takes effect | Swap the binary on disk now; switch the running UI and daemon over at a safe point the user chooses. Auto-update never ends a live pane.   |
| Verification                | The release manifest's GitHub provenance attestation is verified in-process (Sigstore), and the archive's SHA-256 must match the manifest. |
| Platforms                   | Linux, macOS (Apple silicon) and Windows.                                                                                                  |
| Live-pane handover          | Out of scope. Filed as a separate Linear exploration project.                                                                              |

## Components

### `thegn-core::update` — pure logic, no I/O

- **Version ordering** by SemVer with pre-release identifiers:
  `0.1.0-alpha.121 > 0.1.0-alpha.120`, and any `beta` sorts above any `alpha`.
- **Install-method detection** from the resolved path of the running executable:

  | Method                         | Detected by                                                       | Action                                                        |
  | ------------------------------ | ----------------------------------------------------------------- | ------------------------------------------------------------- |
  | Release archive                | a user-writable location such as `~/.local/bin` not matched below | self-update                                                   |
  | Nix / home-manager             | path under `/nix/store`                                           | print `nix profile upgrade …` or the home-manager instruction |
  | deb / rpm                      | `/usr/bin/thegn` owned by the package database                    | print the `apt`/`dnf` command for the new asset               |
  | Homebrew                       | `/opt/homebrew/Cellar` or `/usr/local/Cellar`                     | print `brew upgrade thegn`                                    |
  | winget / scoop                 | their install roots, once those channels exist                    | print the manager's command                                   |
  | `just live` install            | `~/.local/lib/thegn`                                              | print the `just live` instruction                             |
  | Source checkout                | path under a Cargo `target/` directory                            | print the rebuild instruction                                 |
  | Anything else, or not writable | —                                                                 | print manual instructions                                     |

- **Target selection** maps the running build to its archive:
  `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-apple-darwin`,
  and `x86_64-pc-windows-msvc` once a Windows archive ships. A build keeps its
  libc flavour (musl stays musl).
- **Verification policy** (constants compiled into the binary): repository
  `blakeashleyjr/thegn`, workflow `.github/workflows/release.yml`, OIDC issuer
  `https://token.actions.githubusercontent.com`, and a signing identity built
  from `refs/tags/v<tag>` for the exact tag being installed.

### `thegn-svc::update` — network and filesystem

- **Checker.** Lists releases from the GitHub REST API, ignoring drafts. A
  release qualifies when it is newer than the running version, allowed by the
  channel policy (an alpha or beta build accepts newer pre-releases; a future
  stable build accepts only stable releases unless
  `[update] channel_prereleases = true`), and carries an archive for this
  platform. The result is cached in the state database as `UpdateState`
  (latest tag, notes URL, checked-at, install method, last error). The check
  runs at most every 24 hours, stale-while-revalidate, with backoff after
  failures, and honours GitHub's rate-limit reset time.
- **Verifier.** Downloads `thegn-<tag>-manifest.json` (capped at 1 MiB),
  fetches its attestation bundle from the GitHub attestation API by the
  manifest's SHA-256, and verifies it with the `sigstore` crate against its
  bundled trust root and the policy above. It then downloads the platform
  archive (capped at 200 MiB, the release pipeline's own cap) and requires its
  SHA-256 to equal the manifest entry.
- **Installer.** Extracts `thegn` into a staging file in the destination
  directory, fsyncs it, runs `<staged> --version` and requires the expected
  version, copies the current binary to `thegn.prev`, then atomically renames
  the staged file into place. Refuses when the directory is not writable.

### Surfaces

- **Daemon:** runs the background check and publishes `UpdateState`.
- **UI:** status-bar chip (`⬆ alpha.121` / `⬆ alpha.121 · restart to finish`),
  an Info-screen entry and a `thegn doctor` line.
- **CLI:** `thegn update` with `--check`, `--yes`, `--restart`, `--rollback`
  and `--to <tag>`.
- **Config:** `[update] check = true`, `auto = false`,
  `channel_prereleases = false`; `THEGN_NO_UPDATE_CHECK=1` disables checks.
  New keys must satisfy the three config-key ratchets.
- **Never checks or updates:** in CI (`CI` set), under `THEGN_E2E` or tests, or
  inside a thegn sandbox. A sandboxed pane or agent cannot trigger an update.

## Update flow

### Check

1. The daemon, at most every 24 hours unless disabled, selects the newest
   qualifying release.
2. It records `UpdateState`. Failures are silent apart from `doctor`, and back off.

### Apply (`thegn update`, or the background job when `auto = true`)

1. Resolve the install method. Every method except the release archive stops
   here and prints its exact upgrade command.
2. Download the manifest and the archive under their size caps.
3. Verify the attestation and the archive digest. Any failure aborts with
   nothing changed.
4. Stage, probe `--version`, keep `thegn.prev`, and swap atomically.
5. Record a pending switchover (see below).

### Platform specifics

|                                      | Linux                                                               | macOS (Apple silicon)                                                                                                                                                                                                     | Windows                                                                                                                                                                            |
| ------------------------------------ | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Swap while running                   | `rename` over the running file; running processes keep the old copy | same as Linux                                                                                                                                                                                                             | a running `.exe` cannot be replaced: rename it to `thegn.exe.old`, move the new file in, delete `.old` on the next start                                                           |
| Package-managed installs, handed off | Nix, deb, rpm                                                       | Homebrew, Nix                                                                                                                                                                                                             | winget, scoop (once they exist)                                                                                                                                                    |
| Platform details                     | —                                                                   | The binary is unsigned and unnotarized. An in-process download carries no quarantine attribute, so Gatekeeper does not intervene. The ad-hoc signature is preserved and checked with `codesign --verify` before the swap. | The install directory must be user-writable; anything under `Program Files` is handed off. Until a Windows archive ships, the check reports that the release has no Windows build. |

### Rollback

`thegn update --rollback` swaps `thegn.prev` back. It refuses once the newer
version has migrated the state database forward (older builds refuse newer
databases); the message says so and links the upgrading guide.

## Switchover and version skew

- After a swap the running UI and daemon keep the old binary. The status bar
  shows `restart to finish`, and `doctor`/Info show the live-pane count.
- A newly started UI compares its version with the running daemon's registry
  entry (`daemon` row `version`). On a mismatch it does not attach. It offers
  **Restart daemon now** (listing the live panes it would end, agents marked)
  or **Keep the old version** (start the UI from `thegn.prev` to match).
- `thegn update --restart` swaps and switches over in one step after the same
  confirmation; `--yes` skips the confirmation for scripts.
- `auto = true` downloads, verifies and swaps in the background, notifies
  ("alpha.121 installed — restart to finish", with the notes link), and
  switches over only when no panes are live: the daemon's idle restart or its
  next start. It never ends a live pane itself.
- The first start of the new version may migrate the state database forward;
  `thegn update` states this before swapping.

## Errors

Every failure leaves the installed binary untouched; leftover staging files
are removed on the next run. Typed errors, each naming its fix: `network`,
`rate-limited`, `no-asset-for-platform`, `attestation-invalid`,
`digest-mismatch`, `not-writable`, `managed-install` (with the command to
run), `version-probe-failed`. `attestation-invalid` and `digest-mismatch` are
reported prominently and never retried automatically.

## Privacy

Network traffic: `api.github.com` for the release list; when updating, the
manifest, its attestation and one archive. No identifiers; User-Agent
`thegn/<version>`. `docs/help/privacy.md` replaces "thegn does not check for
new versions" with an exact description of this traffic and how to disable
it, and the first run prints a one-line notice.

## Testing

- `thegn-core` unit tests: version ordering including pre-release identifiers,
  install-method detection from path fixtures, target selection.
- Verifier tests against **recorded real fixtures** from a published release
  (its manifest and attestation bundle), plus negative cases: tampered digest,
  wrong repository, wrong workflow, wrong tag. No stubbed `gh`; the alpha.120
  upload failure got past the gate because the release tests stubbed it.
- Installer tests in temp directories: swap, `.prev`, rollback, refusal when
  not writable, and the Windows rename-aside path (compiled under the Windows
  cross-check).
- A live end-to-end check, not run on every push: `thegn update --check` and a
  full update into a scratch directory against the real latest release.

## Out of scope

- **Live-pane handover to a new daemon** (passing PTY handles and restoring
  session state across processes, so a switchover never ends a pane). Filed as
  a separate Linear exploration project.
- Delta or partial downloads, mirrors, and package-manager publishing.
