# Design

## One resolver for launch and diagnosis

Extract a `StageWorkerProbePlan` from the normal stage-worker launch resolver
after configuration layering is complete. The plan records, without secrets:

- pipeline/stage identity and the resolved agent/command identity;
- environment-key provenance and the relocated provider-home mount set;
- resolved outer backend/profile and inner harness isolation mode;
- worktree, per-worktree Git dir, Git common dir, and shared-config protection;
- the launching executable exported as `THEGN_BIN` and its in-sandbox target;
- supported dynamic checks and the reason for any unsupported check.

Production launch and doctor must call the same resolver. Doctor may substitute
only the payload command and disposable worktree/Git/sentinel roots after
resolution. It retains the resolved provider-home sources but checks only their
existence, intended mount mode, and in-sandbox visibility; it never opens their
credential contents. Doctor must not maintain a second approximation of env
precedence or mount selection. Sensitive env values and credential file
contents never enter diagnostics.

## Non-billable probe payload

The probe payload is a local, deterministic helper—not the resolved agent
command. It runs under the resolved outer backend/profile with the stage
environment shape and verifies:

1. the worktree accepts a file write;
2. Git's per-worktree index/object/ref paths accept `git add` and `git commit`;
3. the resulting `HEAD` contains the probe file;
4. shared `.git/config` retains its intended protection;
5. `THEGN_BIN` resolves to the launching executable and can execute a local
   non-networked identity/version probe;
6. each required provider-home source exists and is visible with its intended
   access mode, without opening credential contents; and
7. a purpose-created sibling path outside the allowed worktree/Git paths rejects
   a write.

Doctor uses a temporary worktree/Git/sentinel topology and a synthetic empty Git
identity, so the write/commit checks neither touch user history nor depend on
global Git config. Configured provider homes are metadata/visibility checks
only. The probe never starts a provider CLI, model session, forge request, or
network probe.

## Result vocabulary

Each subcheck returns one of `pass`, `fail`, `unsafe`, or `not-proven` plus a
bounded remediation. The aggregate result distinguishes at least:

- `ready/contained`;
- `worktree-not-writable`;
- `git-metadata-not-writable`;
- `callback-unavailable-or-stale`;
- `provider-home-unavailable`;
- `outside-write-allowed` / `containment-absent`;
- `unsafe-inner-full-access-without-outer-containment`; and
- `unsupported-platform-or-backend`.

Only complete dynamic evidence may produce `ready/contained`. Static mount
composition or host-side access can explain a failure but cannot turn
`not-proven` into a pass. Non-Linux platforms and backends without an equivalent
dynamic probe report the unsupported reason explicitly.

## Admission enforcement

Automated pipeline stage admission evaluates the resolved inner/outer isolation
pair before process launch. An inner harness configured for full access with
outer backend `none` or another uncontained result is an unsafe configuration,
not graceful degradation: the stage is placed on an infrastructure hold with an
actionable diagnostic. Doctor reports the same decision and resolution
provenance. This rule is scoped to automated stage workers; it does not silently
change the general interactive-pane fallback contract.

## Disposable Linux/bwrap smoke

The end-to-end fixture creates a private temporary root with:

- a repository plus a linked worktree, exercising the real `.git` indirection,
  per-worktree Git dir, and Git common directory;
- an isolated fake home and provider-home directory;
- a sibling `forbidden` sentinel made visible but read-only through the composed
  sandbox; and
- an explicit local Git author/committer identity.

The test invokes the normal bwrap composition and the deterministic probe
payload. It requires `git add`, `git commit`, and `git show` to succeed inside
containment, requires the sentinel write to fail, and verifies both outcomes
from the host. A scoped RAII/tempdir owner tears down every path and process on
success, failure, and panic. The fixture skips only with an explicit unsupported
reason when Linux user namespaces/bwrap are unavailable; CI lanes designated to
prove bwrap treat that condition as a failure.

## Shared probe substrate

Backend execution, redacted result framing, timeouts, and unsupported-state
reporting may be shared with THE-90's contained sccache check. The checks remain
separate doctor rows and result types: a worker may be safely commit-contained
while its optional compiler cache is unreachable, or vice versa.
