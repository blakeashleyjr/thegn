---
title: Publish alpha.118 and hand Windows/WSL testing to Pritam
type: feat
status: active
date: 2026-11-22
---

# Publish alpha.118 and hand Windows/WSL testing to Pritam

## Overview

Land the in-progress RFC/board-audit rewrite, cut a real published release from
the local `main` tip, and hand Pritam a WSL2 install path plus a Windows-native
candidate with a test checklist. The release is the gating item: today nobody
outside this checkout can download a thegn binary.

---

## Problem Frame

The local checkout holds a large, unreleased body of work and a piece of
uncommitted documentation:

- `main` is `359628f9b` and is **803 commits ahead of `origin/main`**
  (`7a0f0e450`). The remote tip is exactly `v0.1.0-alpha.103`.
- Only two tags exist (`v0.1.0-alpha.1`, `v0.1.0-alpha.103`) while
  `Cargo.toml` says `0.1.0-alpha.118` — 15 version bumps were never tagged.
- Nothing is published. `gh release list` shows two **drafts**; the
  `v0.1.0-alpha.103` release run failed at `package`
  (run 44638984050: the packaging job resolved the `rpm` **man** output and
  `rpm2cpio` was missing), so `verify-manifest` never ran. That specific bug is
  already fixed on local `main` by commit `247766441` ("fix(release): select
  the out output when locating packaging tools"), but no release has been
  attempted since.
- Uncommitted content: rewrites of `docs/rfcs/agent-execution.md`,
  `docs/rfcs/agent-tier-local-mode.md`, `docs/rfcs/jobs.md` (first release =
  local terminal + project-selected Fly Sprites / Vercel Sandbox runner), plus
  new `docs/audits/2026-11-18-thegn-linear-board.md` and its 2,437-row
  `-register.csv`.

Pritam cannot test on Windows/WSL until the first item is solved: a WSL2 user
installs the published Linux x86_64 tarball, and there is none. Windows-native
is explicitly deferred (`KNOWN_ISSUES.md`, `README.md`), has no artifact, and
its opt-in CI job has never produced a recorded run
(`openspec/changes/add-windows-ci-distribution/tasks.md` §4.3 unchecked).

---

## Requirements Trace

- R1. The RFC/audit content is committed and pushed to `origin/main`.
- R2. A tagged, **published** GitHub release exists for the current alpha whose
  assets download and verify (archive, checksums, attestations, deb/rpm,
  manifest).
- R3. The release notes explain, in user-visible terms, what changed since
  `v0.1.0-alpha.103` — 803 commits of it.
- R4. Pritam gets a documented WSL2 install + test path using published assets.
- R5. Windows-native status is stated truthfully, with either a candidate
  `thegn.exe` artifact plus its provenance, or a recorded reason there is none.
- R6. No repo claim (README, KNOWN_ISSUES, OpenSpec) outruns the evidence
  gathered here.

---

## Scope Boundaries

- No product/behavior changes in this plan. If a gate fails, the fix is a
  separate, focused change (this plan only sequences and records it).
- No new install channel: Homebrew/AUR stay deferred, deb/rpm stay standalone
  assets, Windows stays unsupported.
- No batch work: the local `tg/b193`–`tg/b209` branches are out of scope; only
  what is already merged into `main` ships.
- No retirement of the RFC decision records; the audit/register is committed
  as a dated snapshot, not regenerated.

### Deferred to Follow-Up Work

- Vercel/Sprites release-gate evidence (THE-2419): measured laptop CPU offload.
  This plan only makes the release installable, it does not run the gate.
- Native Windows on-machine compositor checklist
  (`openspec/changes/add-windows-compositor-validation/tasks.md` §2): Pritam's
  run collects it; archiving the five `add-windows-*` changes stays blocked
  until it passes.
- WSL bind/mount verification (THE-124, THE-141): Pritam's run is evidence, not
  a fix.

---

## Context & Research

### Relevant Code and Patterns

- `RELEASING.md` — the authoritative release sequence (version bump → changelog
  → land → `just nix-build-full` → `release-artifacts`/`release-verify` → tag).
- `.github/workflows/release.yml` — `admit` (protected `release-admission`
  environment, required reviewers) → read-only build legs → upload broker →
  `create-release` **draft** → `package` → `verify-manifest`. Assets land on a
  draft; a human publishes it.
- `.github/workflows/ci.yml` — `on:` is `pull_request`, nightly `schedule`, and
  `workflow_dispatch`. **Nothing runs on a push to `main`.** `windows`,
  `windows-artifact-smoke`, `macos`, `e2e` are opt-in behind `extras: true`;
  `e2e` is known-red (stale baselines), so a dispatch run's overall conclusion
  is not the Windows verdict — read the `windows` job.
- Branch rulesets: `protect main` requires a PR and 90% coverage, but the admin
  role has `bypass_mode: always`, so a direct 803-commit push is permitted.
  `Immutable alpha release tags` restricts update/deletion, not creation.
- `justfile`: `release-artifacts`, `release-verify` (refuses a `/nix/store`
  interpreter unless `host_only=1`), `nix-build-full`, `changelog-lint`,
  `doc-check`, `delivery-check`, `test-windows-vm`, `windows-vm-*`.
- `.git/hooks/pre-push` (prek) runs `just test` + clippy + shellcheck, so the
  push itself is a gate; plan for it to be slow or to run gates first and
  record the same result.
- WSL context: `docs/superpowers/guides/windows-tailscale-docker-hosts.md`
  (WSL2 + Docker integration, systemd, worktrees in the WSL filesystem);
  `docs/help/sandbox-backends.md` (the `wsl` backend is **reserved**, never
  selected); `config/config.toml.example` (OCI entries decline on native
  Windows — "use WSL2 for container sandboxes").
- Windows status sources: `README.md` platform table, `KNOWN_ISSUES.md`
  "Windows is deferred", `CONTRIBUTING.md` "Windows (native) notes",
  `openspec/changes/add-windows-*/tasks.md`.

### Institutional Learnings

- `v0.1.0-alpha.103`'s failure is the instructive one: the release `package`
  job did real work (deb + rpm built) and then died on `rpm2cpio` resolving to
  the derivation's man output. The `^out` fix is in `main`; a rehearsal that
  only builds archives would not catch a regression here, which is why the
  first real run must be watched at the `package` job.
- A Nix-shell build is not a portability rehearsal: `release-verify` refuses a
  store-interpreter binary, and only CI proves the musl/darwin legs.

---

## Key Technical Decisions

- **Tag `v0.1.0-alpha.118`, do not bump.** `.118` is bumped, dated in
  `CHANGELOG.md`, and has never been tagged or published; the docs commit can
  land under it. Bumping to `.119` buys nothing and re-dates a section that
  already reads correctly. (If the docs are restructured into their own
  user-visible change, revisit.)
- **Publish `main` as one push, before tagging.** The tag's `admit` job checks
  out `refs/heads/main`, so the release commit must already be on the branch.
- **Fix red gates before pushing, not after.** A push to `main` triggers no CI,
  so a red push is invisible until the 05:17 UTC nightly; the local gate run is
  the only fast signal.
- **Rehearse locally, expect `host_only=1`.** In `nix develop` the rehearsal
  binary links to the store, so `release-verify` needs `host_only=1` with its
  loud warning — that is expected, not a failure, and CI covers real
  portability.
- **WSL2 is the supported testing path for Pritam; Windows-native is a
  candidate.** Hand over the musl tarball (static, distribution-independent)
  for WSL2, optionally the gnu tarball too. Treat `thegn.exe` from the opt-in
  CI job as a candidate artifact with 30-day retention, never as an install
  channel.
- **Checklist lives in the repo, handoff lives in Linear.** A dated tester
  guide under `docs/superpowers/guides/` is reviewable and versioned; the
  Linear comment carries the links, credentials expectations and what "good"
  looks like for the run.

---

## Implementation Units

- [x] U1. **Land the RFC and board-audit content**

> Done in `2edfc0989` (RFCs), `c2fb6b084` (audit + register) and `f085ba9af`
> (this plan). The cross-references resolve; `treefmt --ci` reported two
> unformatted files that were then formatted.

**Goal:** Commit the RFC rewrite and the 18 November board audit as an
internally consistent, link-clean documentation change.

**Requirements:** R1, R6

**Dependencies:** none

**Files:**

- `docs/rfcs/agent-execution.md`
- `docs/rfcs/agent-tier-local-mode.md`
- `docs/rfcs/jobs.md`
- `docs/audits/2026-11-18-thegn-linear-board.md` (new)
- `docs/audits/2026-11-18-thegn-linear-register.csv` (new)
- `docs/plans/2026-11-22-001-feat-alpha-118-release-and-windows-wsl-handoff-plan.md`
  (new — this plan, committed so the release has a record of what was checked)
- `CHANGELOG.md` (only if a user-visible line is warranted — see below)

**Approach:** The three RFCs now agree that the first release is local terminal

- project-selected Sprites/Vercel runner, with hibernation, Pi/V8/WASM and
  hosted infra as later gates. Before committing, read the three files for
  cross-references that still describe the old "local Job first" sequence, and
  confirm `agent-execution.md#experiment-hibernate-a-stock-cli-between-turns`
  resolves from `agent-tier-local-mode.md`. The board audit links its CSV
  relatively, so both commit together. These are process/architecture documents,
  not user-visible product behavior: the changelog convention
  (`docs/changelog-format.md`) argues for no entry, and a `THE-`/`batch` label in a
  new heading would be rejected by `test/changelog_lint.py`. If a line is added,
  put it under the existing `[0.1.0-alpha.118]` section with no issue id.

**Test scenarios:** Test expectation: none — documentation only. Verification is
the gate run in U2 (`doc-check` covers rustdoc; the markdown formatter covers
tables) plus a manual link/reference read.

**Verification:** `git status` clean; the three RFCs and both audit files are in
one or two commits with messages that say what changed for the reader.

---

- [x] U2. **Establish the release gate on local `main`**

> Run at `2edfc0989`. `lint`, `build`, `check-cross`, `check-features`,
> `check-msrv`, `test-doc`, `doc-check`, `openspec-validate`, `smoke`,
> `sandbox-e2e-db`, `term-check` and `nix-build` passed first time. Two stages
> failed and were fixed: `deps-audit` on RUSTSEC-2026-0592 (`86bd5c94f`) and
> `test`/`coverage` on the host instance's `THEGN_CONFIG_DIR` (`9c4a35f48`).
> With those in, `test` ran 23,132 tests green and `coverage` met the core 95%
> line gate. The two stages were re-run with the host's `THEGN_*` variables
> cleared, which is what CI sees; the earlier failures were the ambient second
> instance, not the code.

**Goal:** Know whether the 803-commit tip is releasable before it becomes
`origin/main`.

**Requirements:** R2, R3

**Dependencies:** U1

**Files:** none expected; fixes, if needed, get their own commits and units.

**Approach:** Run the full local gate in `nix develop`: `just ci` (lint,
deps-audit, build, check-cross, check-features, check-msrv, test, test-doc,
doc-check, openspec-validate, coverage, smoke, sandbox-e2e-db, term-check,
nix-build). Then `just changelog-lint` and `just delivery-check`, which are
outside `ci`. The nightly failures on the _old_ remote tip were `doc`,
`coverage` and `test`; `0.1.0-alpha.118` claims the docs build was repaired, so
treat a `doc-check` failure as a regression to fix, not a known state. Record
per-stage timings so a re-run after U3 is cheap to reason about. Do not start
U3 until this is green or every remaining red is a named, accepted risk.

**Execution note:** Treat this as characterization of the current tip: if a
stage is red, capture the failing unit and decide fix-now vs. accept-and-record
before touching the version.

**Test scenarios:** Test expectation: none — this unit _is_ the test run.

**Verification:** Every `just ci` stage green, or a short written list of the
red stages with the failing test names, the commit they broke at, and why the
release can proceed (or the decision to stop).

---

- [x] U3. **Finalize `CHANGELOG.md` and the version for `0.1.0-alpha.118`**

> No rewrite was needed: `.104`–`.117` already document their own releases, and
> nothing user-visible landed after the `.118` bump commit `06c776669`. The
> section gained the `url` Security line and its date moved to the release day.
> `[0.1.0-alpha.118]` and compare links were already present; the `[Unreleased]`
> link resolves once the tag exists.

**Goal:** A release section that describes 803 commits of user-visible change,
with the tag/version invariant intact.

**Requirements:** R2, R3

**Dependencies:** U2

**Files:**

- `CHANGELOG.md`
- `docs/changelog-format.md` (read only — conventions)
- `.github/workflows/release.yml` (read only — `admit` compares tag to
  `Cargo.toml`)

**Approach:** The `.118` section currently carries only the docs-build fix from
`06c776669`. Add the user-visible highlights since `.103` — the remote-worktree
background shell-ready phases, remote parity harness, provider conformance
cases, `land` sandbox-reporting fixes, session id/attach corrections — in
Keep-a-Changelog categories, written as outcomes, no issue ids, no "batch",
no measured figures. Do not add `compare/` link entries unless the same commit
adds them for the whole `.104`–`.118` run; a hole there is cosmetic and not
worth a release-blocking format decision. Leave `Cargo.toml` at
`0.1.0-alpha.118`.

**Test scenarios:** Test expectation: none — prose plus the existing lint.

**Verification:** `just changelog-lint` green; the `.118` section reads as one
release a user could scan; `cargo metadata` version still `0.1.0-alpha.118`.

---

- [x] U4. **Local release rehearsal**

> `nix-build-full` green; `result/bin/thegn --version` reports the workspace
> version. Rehearsed twice: `v0.1.0-alpha.118` (sha256 `afc0e238…`) and, after
> the `.119` bump, `v0.1.0-alpha.119` (sha256 `56b9850f…`). Both passed
> `release-verify host_only=1` with the expected store-interpreter warning.

**Goal:** Prove the archive this host would ship is well-formed before a tag
exists.

**Requirements:** R2

**Dependencies:** U3

**Files:**

- `target/release-artifacts/thegn-v0.1.0-alpha.118-<host>.*` (generated)
- `packaging/release.json`, `packaging/release.py`,
  `packaging/release_rehearsal.py` (read only)

**Approach:** `just nix-build-full` for the shipped `.#default` path (the
bridge is not built by the routine gate), then
`just release-artifacts v0.1.0-alpha.118` and
`just release-verify v0.1.0-alpha.118 host_only=1`. Run the produced binary and
record `thegn --version` / `thegn doctor` behaviour for the notes. Note
explicitly that musl, darwin, deb/rpm and attestations are unrehearsable
locally — they are proven by the tagged run in U5.

**Test scenarios:** Test expectation: none — this is a rehearsal of the real
validator.

**Verification:** `release-verify` passes with the documented host-only
warning; archive contains `thegn`, `LICENSE-MIT`, `LICENSE-APACHE`,
`README.md`; `nix-build-full` succeeds.

---

- [x] U5. **Push `main`, tag, and publish the release**

> `main` pushed at `ea2d63c13` with tag `v0.1.0-alpha.118`; the admission gate
> was approved and the run built and uploaded every asset, then failed at
> `verify-manifest`: `release.py` rejected the package job's `SHA256SUMS`
> digest because only `thegn*` names were allowed. `verify-manifest` checks out
> the admitted commit and the .118 tag is immutable, so the fix could not
> repair it in place: `29f0fc67e` widens the name rule (with a regression
> test), bumps the workspace to `0.1.0-alpha.119`, and re-tags. `main` and
> `v0.1.0-alpha.119` are pushed; the .118 draft stays unpublished.

**Goal:** Turn the local tip into the public download Pritam can use.

**Requirements:** R1, R2, R3

**Dependencies:** U4

**Files:**

- `Cargo.toml` / `Cargo.lock` (only if U3's final read finds a mismatch)
- `.github/workflows/release.yml` (read only)

**Approach:** Push `main` (pre-push hook runs `just test`/clippy again — either
let it, or push with the hook satisfied by U2 and say so), then
`git tag -a v0.1.0-alpha.118 -m "thegn v0.1.0-alpha.118"` and push the tag.
Approve the `release-admission` environment prompt. Watch `admit`,
`create-release`, the three build legs, `upload`, `package`, `verify-manifest`;
`package` is the job that failed last time, so read its `rpm2cpio`/`cpio` step
directly. Then fill the draft body from the `.118` section (user-facing summary
first, verification instructions second: `gh attestation verify`,
`sha256sum -c SHA256SUMS`), and **publish** the draft. Publishing is the step
that makes assets visible — a green workflow alone leaves them on a draft.

**Test scenarios:**

- Happy path: all jobs green, draft published, every asset present.
- Failure path: `package` red again — capture the failing step and command
  before re-dispatching; do not publish a partial asset set.
- Edge: tag pushed but `admit` rejects version/tag mismatch — fix forward by
  deleting the tag locally/remotely only if it was never admitted; otherwise
  cut the corrected version.

**Verification:** `gh release view v0.1.0-alpha.118` shows `isDraft: false`,
non-empty body, and the full asset set; a clean directory download plus
`sha256sum -c SHA256SUMS` succeeds; one attestation verifies.

---

- [ ] U6. **Clean-host install verification**

**Goal:** Prove the published bytes run somewhere that is not this checkout.

**Requirements:** R2, R4

**Dependencies:** U5

**Files:** none (evidence goes to the issue handoff)

**Approach:** On a scratch Linux container or VM, download the musl archive,
verify its checksum and attestation, unpack, and run `thegn --version`,
`thegn --help`, `thegn doctor`. The musl archive is the one Pritam will use, so
verify that one first. Record the exact asset name, digest and observed output.

**Test scenarios:** Test expectation: none — this is the verification.

**Verification:** A recorded transcript that a fresh Linux host installed the
musl asset and ran `thegn doctor` without a Nix store or this repo present.

---

- [x] U7. **Windows-native evidence: opt-in CI run and artifact**

> Dispatching `ci.yml` with `extras: true` (run 45091904082) answered it: the
> MSVC workspace check passed, then the `thegn-svc` lib test target failed to
> compile (`calendar/command_tests.rs:110` and `inbox/command_tests.rs:324` use
> `libc::kill` without a `cfg(unix)` guard). The IPC/Job-Object tests, the
> `thegn.exe` build and the artifact smoke were skipped, so no candidate
> artifact exists. Recorded in `CONTRIBUTING.md` and the
> `add-windows-ci-distribution` task; no support claim changed.

**Goal:** Either give Pritam a provenance-carrying `thegn.exe` candidate or a
recorded "not yet" that closes the ambiguity.

**Requirements:** R5, R6

**Dependencies:** U5

**Files:**

- `.github/workflows/ci.yml` (read only — `windows`, `windows-artifact-smoke`)
- `openspec/changes/add-windows-ci-distribution/tasks.md` (§1.1, §4.3)
- `openspec/changes/add-windows-compositor-validation/tasks.md` (§2)
- `CONTRIBUTING.md`, `KNOWN_ISSUES.md`, `README.md` (only if the result changes
  a claim)

**Approach:** Dispatch `ci.yml` with `extras: true` against `main`. Read the
`windows` and `windows-artifact-smoke` jobs individually — the run's overall
conclusion is expected red because `e2e` is opt-in and stale. If the Microsoft
leg is green, tick phase-6 1.1/4.3 with the run link and download the
`thegn-x86_64-pc-windows-msvc` artifact (30-day retention) for Pritam. If it is
red, capture the failing test or compile error; do **not** flip any support
claim, and leave the OpenSpec tasks unchecked with the run link as the new
baseline. Do not start the local KVM path (`just windows-vm-setup`) inside this
plan — it is a multi-hour, license-accepting, 6 GiB download and a separate
decision.

**Test scenarios:** Test expectation: none — the CI job is the test.

**Verification:** A recorded run link with the two job conclusions; either an
artifact digest handed over, or the exact red step named.

---

- [x] U8. **Pritam's WSL2 + Windows handoff**

> `docs/superpowers/guides/windows-wsl-test-handoff.md` written, plus a WSL
> pointer in `KNOWN_ISSUES.md`. The Linear comment could not be posted from
> this environment (no Linear credentials); its text is in the handoff reply.

**Goal:** A tester can install, run and report without asking follow-up
questions.

**Requirements:** R4, R5, R6

**Dependencies:** U5, U6, U7

**Files:**

- `docs/superpowers/guides/windows-wsl-test-handoff.md` (new)
- `docs/superpowers/guides/windows-tailscale-docker-hosts.md` (link, do not
  duplicate)
- `openspec/changes/add-windows-compositor-validation/tasks.md` (reference the
  checklist, do not edit its checkboxes)

**Approach:** Write one guide with two paths and one checklist.

_WSL2 path (supported):_ `wsl --install`, a distro with `git`; unpack the
**musl** tarball into `~/.local/bin`; `thegn --version`, `thegn doctor`; enable
`systemd=true` in `/etc/wsl.conf` and `loginctl enable-linger` for the
`systemd` backend, or install `docker-ce`/podman **inside** the distro for
`docker`/`podman-*`; keep repositories and worktrees on the Linux filesystem
(`~/…`), never `/mnt/c`, because the OCI mount check resolves `-v` inside the
WSL VM; expect the reserved `wsl` backend to be rejected by design — the
sandbox is a Linux runtime inside the distro, not a Windows-hosted one.

_Windows-native path (candidate only):_ say plainly that no Windows binary is
published, and offer, in order, the opt-in CI artifact from U7 (with its run
link and expiry) and a source build (`rustup` + VS Build Tools,
`cargo install --path crates/thegn-host`, Windows Terminal, Job-Object process
scoping). Point at the checklist in
`openspec/changes/add-windows-compositor-validation/tasks.md` §2 (waker spike,
first frame, idle CPU, resize storm, Ctrl-C, StderrGuard, conhost refusal,
daemon race, glyphs) and ask for the same shape of evidence on the WSL path.

_Reporting:_ what to attach (version, `thegn doctor` output, terminal, distro
and kernel, whether Docker Desktop or in-distro docker, the failing key sequence
or log line), and where (the Linear issues: THE-141 for real-host WSL
verification, THE-124 for WSL binds, the compositor-validation issue for
Windows UI). No secrets, no model logins, no provider tokens.

Post the same content as a Linear handoff comment with the release tag, the
asset name and digest, and the guide path.

**Test scenarios:** Test expectation: none — documentation. The test is
Pritam's run; treat "asked a question the guide should have answered" as a
guide defect to fold back in.

**Verification:** A reviewer can follow the WSL path end to end on a fresh
distro using only the guide and the published release; every claim in it is
backed by U5/U6/U7 evidence; no repo document claims Windows is supported.

---

## Risks & Mitigation

| Risk                                                         | Impact                                                 | Mitigation                                                                                                |
| ------------------------------------------------------------ | ------------------------------------------------------ | --------------------------------------------------------------------------------------------------------- |
| `just ci` is red at the tip (nightly was red on the old tip) | Blocks the tag                                         | U2 before U3; fix or explicitly accept each red stage                                                     |
| Release `package` job fails again on packaging tools         | No published release (exactly today's state)           | Read `package` step-by-step during U5; the `^out` fix is already in `main`, but the job has not run since |
| Push is 803 commits and the pre-push hook re-runs the suite  | Slow, or a red hook blocks the push                    | U2 first; if the hook disagrees with U2, treat it as a real signal, not a bypass candidate                |
| Windows CI leg is red or flaky                               | No candidate `thegn.exe`                               | Hand WSL only; record the failure against the OpenSpec task; do not weaken the "deferred" claim           |
| Release published without notes                              | Pritam has no "what changed" and no verification steps | Notes are written into the draft **before** publishing, in U5                                             |
| WSL user hits `/mnt/c` bind failures                         | Looks like a thegn bug                                 | Guide calls out the Linux-filesystem requirement and the mount-check remedy text                          |

---

## Open Questions (decisions assumed above — confirm or override)

1. **Version:** tag `v0.1.0-alpha.118` (assumed) vs. bump to `v0.1.0-alpha.119`
   because a new commit lands after `.118`'s release commit.
2. **Windows scope for Pritam:** WSL2 supported + candidate `thegn.exe` from a
   one-off opt-in CI run (assumed) vs. WSL2 only, deferring even the CI
   experiment.

---

## Verification Summary

The plan is done when: `origin/main` contains the RFC/audit content; a published
`v0.1.0-alpha.118` release exists with verified assets; a clean host installed
the musl archive; the Windows CI leg has a recorded verdict with either an
artifact or a named failure; and Pritam has one guide and one Linear comment
that let him run the checklist without asking what to install.
