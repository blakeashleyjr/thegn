# THE-314 + THE-315 plan (GitHub issue backend)

Scope: crates/thegn-svc/src/issue/{github.rs, gh_run.rs (new), mod.rs (mod decl)}.

THE-314: `gh` runs through a bounded runner (gh_run.rs) on spawn_blocking: own process
group (killed on deadline/overflow/cancel and after exit), per-stream and total byte caps,
deadline, process-wide concurrency semaphore, drop-guard cancellation, typed errors
(not installed / timeout / truncated / nonzero), stderr redaction + cap, auth vs api
classification. The THE-251 shared primitive does not exist on main, so this is a local
runner built on plugin::proc::{set_process_group,kill_group}.

THE-315: one scope resolver: explicit (draft.project_id / issue identity) > configured
`--repo` flag > worktree dir inference; mutations (create, update) fail closed when none.
Conflicting repeated --repo flags rejected. create passes --repo, verifies the printed URL
owner/repo equals the requested scope, re-fetches with the exact scope. Legacy bare ids
pick up the configured repo. Tests use a fake `gh` script via a `program` field.

Out of scope: forwarding arbitrary extra_flags to create (list-only flags would break it),
changing precedence of filter.repo vs configured flags, capability ledger (no capability change).
