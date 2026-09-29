# Add the pipeline supervisor's policy layer (phase 1 of 4)

## Why

`[[pipeline.stages]]` is an org chart with no engine. thegn validates and
displays it and deliberately advances nothing: `add-agent-orchestration-surface`
rejected a native drain driver because _"every driver feature hard-codes
judgement the prompt should own"_.

That decision is right about judgement and wrong about everything else. Measured
across four maintenance batches on a live pipeline, what a supervising agent
actually spends its time on is:

- running a compile/test/lint pass, because **a stage worker cannot** — the
  pipeline sandbox bind-mounts the Nix store read-only, so `nix develop` cannot
  materialise a shell inside it, and a worker's `implementation-ready` therefore
  means _source-reviewed, never built_;
- classifying what came back;
- dispatching the next stage when the preconditions everyone already agreed on
  are met;
- handing a finished lane to the merge queue.

None of that is judgement, and all of it stops the moment no agent session is
open. The interim answer was a ~300-line shell script whose wake chain died with
its terminal and which shipped five bugs found only by running it: it filed
already-landed lanes as findings, read a test failure as a compile error, and
re-ran a nine-minute validation because a row's status changed under an
unchanged tree. Those are not shell problems. They are the problems of a policy
that was never written down as a checkable thing.

## What Changes

A **pure policy layer** in `thegn-core`, plus the storage and the read-only
verbs to inspect it. Nothing in this change executes anything.

1. **`pipeline_validate`** — classify one validation run into `green`,
   `compile-error`, `test-failure`, `lint-finding`, `inconclusive` or
   `environment-error`. Composes `gate::classify_exit` for the ran-vs-could-not-run
   split; adds output classification keyed by the `[[tasks]]` entry's `matcher`,
   so it is toolchain-agnostic. Caps the stored digest.
2. **`pipeline_approval`** — an approval is a record **bound to a commit**.
   Approve a diff, push one more commit, and it reports `superseded`.
3. **`pipeline_supervise`** — the planner: lane facts in, one action per lane
   out (`validate` / `advance` / `enqueue` / `escalate` / `hold`). Every
   mutating action carries the requirements that authorized it and cannot be
   constructed without them.
4. **Config** — per-stage `validate` (names `[[tasks]]` entries, never shell
   commands) and `requires` (a closed requirement vocabulary), plus
   `[pipeline.supervisor]`, off by default. `config validate` refuses an
   **unsatisfiable** requirement rather than letting a lane park invisibly.
5. **Schema v70** — `pipeline_validations` and `pipeline_approvals`, both keyed
   on a commit.
6. **`thegn supervise plan|status|validations`** — read-only. `plan` prints what
   the supervisor _would_ do and the recorded facts that would authorize each
   action, so an operator can read its behaviour against their own chart before
   switching it on.

## Why this does not reopen the rejected driver

`daemon/pipeline_reaper.rs` already carved the exception, for one verdict: the
daemon may apply a transition when _"applying it is arithmetic on recorded
facts, not a judgement about whether the work was any good"_, under the standing
rule that **the daemon can park a row but never finish one**.

This change generalizes that rule without weakening it. The judgement — _should
this advance?_ — is the operator's, written once per stage in `requires`, in the
config file, in advance. The planner only checks recorded facts off against it.
Three prohibitions are structural rather than disciplinary:

- **No action creates an approval.** Granting is an operator-scoped write; the
  planner has no variant for it.
- **No approval survives a new commit**, because approvals are keyed on one.
- **No action writes a quality verdict on a row.** There is no `Fail` variant;
  the existing artifact+report done-gate is untouched.

## Impact

- **Roadmap**: the agent-orchestration line (the `maintenance-*` chart this
  drives), and the merge-queue line it hands lanes to.
- **Specs**: `agent-orchestration` — ADDED (validation classification, approval
  binding, the transition gate, unsatisfiable-config refusal).
- **Config**: additive. `[pipeline.supervisor]` is depth 2, so it is out of
  env-overlay scope and has no nix mirror; `[[pipeline.stages]]` gains two
  optional fields. A config with none of them behaves exactly as before.
- **Schema**: 69 → 70, additive and idempotent. Must be migrated by the
  **release** binary (`target/debug/thegn` refuses, and letting it migrate would
  break the running daemon).
- **Follow-on phases**: (2) run validations and record them; (3) approvals,
  advance, enqueue, the daemon task; (4) capability-catalog rows and every
  surface, the `pipeline` event frame, automation event kinds, the board column.
