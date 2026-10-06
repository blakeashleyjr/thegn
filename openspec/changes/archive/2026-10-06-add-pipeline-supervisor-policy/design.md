# Design — pipeline supervisor, phase 1 (policy)

## The line this change draws

thegn rejected a native drain driver because "every driver feature hard-codes
judgement the prompt should own". That is the constraint, not an obstacle: the
design succeeds only if **no judgement is hard-coded here**.

The mechanism is that the judgement is _data the operator writes_. Each stage
declares, in `config.toml`, what must be recorded about the output it consumes
before it may start. The planner never forms an opinion; it checks recorded
facts against that list. `daemon/pipeline_reaper.rs` already established the
principle for one verdict — apply a transition when "applying it is arithmetic
on recorded facts" — and this is that principle with the facts and the
requirements both made explicit.

Three prohibitions are structural, not disciplinary:

| Prohibition                                | How it is enforced                                                                                         |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------------------- |
| never manufacture an approval              | the `SuperviseAction` enum has no variant for it; granting is an operator-scoped write outside the planner |
| never honour an approval after new commits | approvals are keyed on a commit; `state_for` returns `Superseded`                                          |
| never write a quality verdict on a row     | there is no `Fail` action; the existing artifact+report done-gate is untouched                             |

## Module layout, and why each is separate

- **`pipeline_validate`** — exit status + output → class. Separate from the
  planner because classification is where the expensive mistakes live, and it
  must be testable against captured output with no roster, no git, no config.
- **`pipeline_approval`** — the record and its validity. Separate because
  "does this approval still apply" is asked by the planner, by the CLI, and
  (phase 4) by every surface; one answer, one place.
- **`pipeline_supervise`** — the planner. Consumes both.
- **`db_pipeline_supervise`** — the two ledgers, a sibling `impl Db` block so
  the pinned `db.rs` keeps only DDL (the arrangement `db_dispatch.rs` uses).

## Decisions and the alternatives rejected

**Validation names `[[tasks]]` entries, not commands.** A `command` field would
be arbitrary command execution from config, which is the same door
`PipelineStage::agent` is deliberately closed against. Naming tasks also makes
the feature toolchain-agnostic for free: `[[tasks]]` already carries `matcher`
values for pytest, go-test, jest, junit and the rest, so nothing here is
Rust-specific.

**Exactly one action per lane per pass.** The alternative — emit every
applicable action — makes a plan unreadable and makes "did the supervisor see
this lane?" unanswerable. A lane omitted from a report is indistinguishable
from a lane never examined, so even `Hold` is emitted, with its reason.

**`Authorization` is a newtype, not `Vec<Requirement>`.** A stage that declares
no `requires` advances with no gate — which may be exactly what the operator
wants and is the most consequential thing a chart can say. A bare vector invites
`join(", ")`, which prints an empty string. This was not theoretical: the first
run against a real chart printed `authorized by: `, reading as a rendering bug
rather than as an ungated transition. `Authorization::describe` cannot return
empty.

**A retryable class is retried twice, then reported as the machine.** A gate run
on this hardware has gone red purely from CPU contention, and re-running it
alone was green; reporting that as a test failure sends an agent to fix working
code. So `EnvironmentError` and `Inconclusive` are retried, and when the budget
is spent the escalation says in words that this is a fact about the machine, not
about the branch. Two attempts, because each is a full compile and an
environment failure that survives one retry is not transient.

**Everything is keyed on the lane's head commit.** Validation records, approval
validity, and action de-duplication. The rejected alternative — keying on the
roster row and its status — was measured: a prototype re-ran a nine-minute
validation because a row moved `running` → `done` under an unchanged tree.

**An unresolvable head is a `Hold`, not an escalation.** Every rule after that
point asks a question about a tree. Found by running the planner over a real
678-row roster: rows from finished batches whose worktrees were since removed
resolve to an empty head, and without the guard each one asked a reviewer to
"approve `''`".

**A merged lane is detected with `merge-base --is-ancestor`, never by diffing.**
A merged lane shows no diff, so a diff-based test reads it as an empty change.
That mistake put four already-landed lanes into a review queue in the shell
prototype.

## What phase 1 deliberately does not do

No execution: nothing runs a validation, dispatches a stage, grants an approval
or touches the merge queue. `[pipeline.supervisor]` is parsed, validated and
read by `supervise plan`, and switches nothing on, so this change is reviewable
as a contract and its planner is exhaustively tested before anything can act on
its output.

## Risks

- **Schema v70 must be migrated by the release binary.** `target/debug/thegn`
  refuses, and letting it migrate would leave the running daemon unable to open
  its own DB. Verification for this phase therefore ran against an isolated
  state home seeded with a copy of the live roster's rows, not against the live
  database.
- **The escalation set is per row, not per lane.** Three rows of one lane
  produce three `awaiting-approval` entries. That is correct as a plan, and is
  the applier's problem to de-duplicate when it starts notifying (phase 3).
