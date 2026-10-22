---
id: merge-queue
title: Merge queue
parent: workflows
order: 2
contexts: [panel:merge]
actions: [integrate, merge-drain, open-merge-queue, sweep-merged]
---

# Merge queue

A **local** merge queue: finished branches queue up, and a fold-actor
lands them on `main` one by one — fold, gate, advance the ref — without
ever checking `main` out. Enable it with `[merge_queue] enabled = true`.

## Queueing

A worktree with pipeline rows is admitted only when its newest row that is not failed or abandoned is finished and has a trusted gate attestation for the enqueued commit. `thegn merge add <worktree> --admin --why <reason>` records an override on that pipeline row.

Add the current worktree's branch from the [[panel]]'s _merge_ section,
the sidebar row menu, or `thegn merge add` in the pane. The queue is
persisted; entries survive restarts. The section shows the active
project's queue by default; `g` widens it to every project.

The project header in the full sidebar also shows a compact queue token when
that project has active entries. It summarizes the highest-priority tier —
blocked, working, or populated — and is a shortcut to this existing Work ▸
Merge queue section. The token is omitted for empty or landed-only queues and
is not shown in rail mode. The bottom-bar summary is opt-in: set
`[bars] bottom_right = ["mq"]` if you want it alongside the other widgets.

## Landing

- **Integrate** (palette, or the section's key) drains the queue once:
  each queued branch is folded into `main` and gated before the ref
  advances; conflicted branches stay queued.
- **Drain (agent autopilot)** hands conflicts to a coding agent to
  resolve, then continues.
  Each eligible row is claimed by the drain working it, so a concurrent drain
  skips that row with a warning; an expired claim on a still-claimable row can
  be taken over after about five minutes.
  A row for a worktree thegn has not registered is claimed only when
  `git worktree list` attributes it to this repository on the queued
  branch; otherwise it is skipped with a warning.
- `thegn merge list` shows the drain holding each row (its owner, claim
  phase and lease), and `--json` adds a `claim` object (`null` when
  unclaimed); the claim token is never shown.
- `thegn integrate` does the same from any shell. It prints the branches
  it is about to fold and asks before folding any of them; `--dry-run`
  prints that plan and stops, and `--yes` skips the prompt (required to
  fold non-interactively).

Only branches you **queued** are folded. `--all`, or `[merge_queue]
require_enqueue = false`, widens it to every _eligible_ worktree branch —
which means every clean branch not already on the target, including work
still in progress. That test cannot tell a finished branch from one you
are midway through, and a land is awkward to walk back even with the
grace period below. Prefer `thegn merge add`.

`thegn merge land` and the section's `l` action land only a local, unclaimed
`ready` queue row with a gate result for the checked-out branch. Otherwise they
refuse with the reason; use `thegn land` to land that branch outside the queue.

- A blocked branch is re-attempted by the next drain automatically. Once
  it has burned its `agent_max_attempts` the agent stops being
  dispatched for it — `thegn merge retry` re-arms held rows only
  (`deferred`, `gate_failed`, `gate_error`, `agent_blocked`, or `needs_human`)
  after you've fixed something, and refuses every other state. The section's
  `r` gesture follows its own retry path.
- A local queue row is bound to the branch tip recorded at enqueue. If the
  branch moves or disappears before a fold, or a fixing agent leaves the
  worktree off that branch, the row is held `agent_blocked` with an
  `identity drift:` reason and no attempt spent. `thegn merge add` re-enqueues
  it against the current tip.
- If a fixing agent does not start, the row is held `agent_blocked` with no
  attempt spent; `thegn merge retry` re-arms it.
- `thegn merge clear` removes unclaimed rows. Rows held by an active drain
  stay queued, are named in the error, and require another clear after the
  drain finishes.
- An unrecognised stored queue status is shown as such and has no land or
  retry key.
- `thegn land` and `thegn integrate` can finish a land while an active drain
  claims the row, but they do not record the landed row and report that it was
  not recorded.
- Deleting a worktree keeps its queue row while an active drain has claimed it.

## Land strategy, signing, drivers

Every land advances the target ref only by an object-DB fold + gate +
compare-and-swap, defers the whole branch on any conflict, and is a no-op
for a branch already merged. Within that, `[merge_queue]` lets you shape it:

- **`land_strategy`** — `merge` (the default 2-parent merge commit),
  `squash` (one single-parent commit with the merged tree), or `rebase`
  (`linear`; the branch's own commits replayed one at a time, keeping
  their original author). `land_message` templates the merge/squash
  subject (`{branch}`, `{target}`, `{subjects}`).
  With `rebase`, a branch containing merge commits is held `needs_human` before
  replay starts; no fixing agent runs. Land it with the merge or squash
  strategy, or linearize the branch.
- **`sign_commits`** — sign the fold/land commits thegn creates. Signing
  is always non-interactive: a locked agent, a missing key, or anything
  that would prompt stops the drain as an _infrastructure_ error with a
  reason — it never marks the branch `needs_human` and never wakes the
  fixing agent, because a signing fault is not the branch's fault.
  Automated commits have a 120-second internal fail-safe: Git is killed and
  the timeout is reported without advancing either ref (on Unix, the isolated
  signer/hook process group is killed too).
  `thegn doctor` probes signing readiness when this is on.
- **`rerere`** — reuse recorded conflict resolutions across drains from the
  queue-private cache, trained from conflict-resolving history and imported
  read-only from the repository's `rr-cache`. The private cache lives under
  thegn's state directory (`$XDG_STATE_HOME/thegn/gate/`); folds never write
  the repository's `rr-cache`. Only resolved pairs are imported. Imports
  include Git's numbered variant resolutions. When the cache exists,
  `thegn doctor` shows its entry and resolved counts; `--json` reports it as
  `source_control.rerere_cache` (`path`, `present`, `entries`, `resolved`),
  null when rerere is off or outside a repository. If the cache is unavailable,
  the fold reports the conflict as usual. A rerere-resolved merge still runs
  the gate before landing.

Folds never run a repository-configured custom merge driver. A conflict on a
path governed by one defers the branch to `needs_human` with
`custom merge driver not run by folds: <path> (merge=<name>); resolve by hand`;
no agent is dispatched. If thegn cannot read the merge configuration, the
fold holds as an infrastructure error.

A mode-160000 conflict is different: the queue defers it as a **submodule
pointer conflict**, naming the path and both ours/theirs object IDs. It never
chooses a pointer automatically and never sends that path through lockfile
regeneration, rerere, or the throwaway merge's blanket stage step. The same
structured message is available to the conflict handoff; ordinary text
conflicts retain the usual fold behavior.

Lockfile regeneration accepts content changes and deletions only within the
conflicted paths. Only the conflicted paths may change: a new path must be a
regular file, an existing path must keep its mode, and deleting one is allowed.
Anything else, or a change to any other path, falls back to the usual conflict
handling. Each accepted regeneration prints one audit line: “merge
queue: regenerated <paths> (approved outputs: exactly the conflict set) with
command `<command>` (timeout Ns) -> tree <oid>”.

## After it lands

`on_landed` decides what happens to a worktree whose branch just landed.
The default is **`expire`**: the worktree is filed into the _Merged_
folder and kept there for `merged_ttl_secs` (7 days out of the box),
then swept away along with its branch.

The grace period exists because the two halves of a land are not equally
recoverable. The branch ref is the merge commit's second parent, so it
costs one command to recreate — but the worktree **directory** holds
gitignored state (`target/`, `.direnv`, env files) that exists nowhere
else. A week is how long you have to notice.

- **Sweep merged** (palette, or `thegn merge sweep`) collects everything
  already past its grace period, now. `--force` ignores the remaining
  time and clears the lot.
  `thegn merge sweep` exits non-zero when a local worktree was refused or only
  partly cleaned up, so scripts can tell. Existing THE-596 branch holds and
  rows that cannot be identified as belonging to this repository are reported
  as informational and do not change the exit status.
- The _merge_ section's **`c`** does the same for that repo after a second
  press: it clears the landed rows and sweeps their worktrees together. Under
  `expire` a landed row _is_ the grace-period clock, so clearing one without its worktree
  would strand the worktree in _Merged_ with nothing left to collect it.
- A merged worktree you have gone back to and **edited is never swept**,
  forced or not. Commit or discard the changes and it becomes eligible
  again.
- The sweep runs at startup and after each land. There is no timer, so
  something that comes due while thegn is closed is collected at the next
  launch rather than at the stroke of the deadline.

Set `merged_ttl_secs = 0` to keep merged worktrees indefinitely (the same
as `on_landed = "move"`), or `on_landed = "remove"` to delete immediately
with no grace period at all.

A one-shot **`thegn land`** (the blessed alternative to `git checkout main
&& git merge`) files the worktree into _Merged_ too, but always **leaves it
in place**: it never removes the worktree or deletes the branch, whatever
`on_landed` says, because it is routinely scripted from _inside_ the worktree
it lands. It also records no queue row, so a worktree it files is **not**
expiry-swept — only branches landed through the queue (a `landed` row) are
collected. Remove such a worktree yourself once you're done with it. (Under
`on_landed = "off"` a `thegn land` instead clears any stale _Merging_ /
_Needs attention_ membership, so a fold-actor land never strands it.)

`thegn land` declares the exact state-DB capabilities needed for the remote
target guard and this folder bookkeeping. It may therefore operate on the
supported older schema without migrating it. If the guard or schema preflight
is unavailable, land refuses before moving git refs. If an I/O failure occurs
only after git has already landed but before the folder update, the command
prints an explicit `sidebar lifecycle bookkeeping unavailable` degraded result
instead of silently claiming that every side effect completed.

The fold advances the ref without checking anything out, so any worktree
sitting **on** the target would be left with a stale working tree. Every
such checkout — the main one or a linked worktree — is fast-forwarded as
part of the land. One with real uncommitted work is left alone and
_named_, with the `git reset --keep` that syncs it: don't commit the
pending deletions `git status` shows there, they are the fold, not a
deletion.

## Across hosts

The queue is **anchored to the target repo** — the host where `main`
lives — because the fold happens inside that repo's object store. You can
still queue branches whose worktrees live on **other machines**: each
row's host is shown as an `@host` chip in the _merge_ section, and at
drain time the branch's tip is bundle-fetched into the target store
before it folds. A branch whose host is unreachable is **deferred** (with
the reason), never silently dropped, and retried on the next drain.

A tip from another host is streamed into private local storage in a bundle
capped at 4 GiB and bounded by the drain's subprocess deadline. A size,
timeout, producer, storage, or validation failure defers the row with the reason.

Run the drain **where the target repo lives**. If you invoke it from a
machine other than the target's host, thegn tells you which host to run
it on — the fold, gate, and ref-advance must be co-located with `main`.

`[merge_queue] remote_mode` chooses how an off-host worktree gets into that
target:

- **`route_to_host`** (the default) keeps the target host authoritative. A
  provider-provisioned environment or user-managed SSH worktree receives the
  host-canonical worktree id, a reachable control URL, and a dedicated
  `merge_add` token. That token can enqueue only its registered worktree; it
  cannot list worktrees or queues, stage/commit, type into a terminal, or clear
  the queue. The host re-reads the registered repo, branch, and remote location
  and uses that location to ingest the tip; it never treats the remote absolute
  path as a host-local path.
- **`push`** folds and gates in the provider clone, advances its local target,
  and pushes that target to `origin`. It needs no inbound host control endpoint,
  but origin—not the host queue—is the convergence authority.

Route credentials live in opaque per-worktree, owner-only runtime files. A
provider receives its file after reusable-image checkpointing through the
provider's secret exec environment. A user-managed SSH target receives the same
two data lines over SSH stdin; the token never enters ssh argv, logs, transcript
text, or an artifact. SSH ownership includes account, host, connection identity,
and worktree, so worktrees sharing one remote `$HOME` cannot replace or revoke
each other. Reattach reuses a still-valid installed credential so an existing
pane keeps working; endpoint changes, expiry, or a binding mismatch rotate it
atomically. Recycling/destruction and SSH worktree deletion revoke the token
before owner metadata disappears. The host persists only the token hash.
An installed route credential is reused only while it has at least seven days
remaining; otherwise it is rotated. SSH reads are capped and time-limited, and
oversized or malformed credential files are refused.

Route mode requires a live `thegn serve` TCP registration and a declared
confidential topology: `[serve] topology = "tls-terminated"` behind a loopback
TLS proxy, or `"tunnel"` for a trusted encrypted tunnel, with a reachable
`advertise_host`/`advertise_port`. Plaintext direct exposure is not eligible.
After changing the endpoint, reprovision or reattach/open the environment to
rotate the credential. A missing endpoint/token, invalid scope, unknown
worktree, stale registered location/branch, or failed remote branch lookup is
an explicit error and never creates a remote-local fallback row. If inbound
secure serving is unavailable, select `remote_mode = "push"` deliberately.

The control URL handed to a remote environment comes only from a live
`thegn serve` daemon in this state directory, and must be a plain origin using
the configured scheme. If live servers advertise different URLs, enqueue
refuses until only one origin is advertised.

For a branch on another host, the drain fetches its tip into a private
per-attempt ref and removes that ref afterwards. Just before moving the target
branch, it reads the source tip again; if it moved, the land stops with a
`source_moved` gate error and retries on the next drain.

## The fixing agent

Point the handoff at an agent either by naming one of your `[[agents]]`
entries — thegn supplies the headless flags for its provider — or by
writing the command yourself:

```toml
[merge_queue]
conflict_handoff = "agent"
agent = "claude"            # an [[agents]] name; thegn adds `-p …`
# agent_command = "..."     # ...or spell it out; this wins when both are set
```

An agent thegn doesn't recognize still works: the prompt is appended as
an argument. With neither key set the handoff quietly becomes "notify",
so a drain says so out loud when `agent` names nothing that exists.

**Write placeholders bare.** Values are shell-quoted for you, so
`-p {prompt}` is right and `-p "{prompt}"` hands the agent a prompt
wrapped in literal quote characters. `thegn config validate` catches it.

`{prompt_file}` (for example, `my-agent --prompt-file {prompt_file}`) passes
the agent an owner-only file under the worktree's git directory instead of
putting the prompt on the command line or in `$THEGN_TASK_PROMPT`. thegn deletes
the file when the run ends; a remote sandbox placement refuses the run.

What the agent is _told_ is yours to change. Leave a prompt empty for
thegn's built-in instructions, or override one per blocker kind:

```toml
[merge_queue.prompts]
conflict = "Fix {branch} for {target}. Conflicts:\n{paths}"
# gate_failure gets {log} instead of {paths}
```

Placeholders are checked against the kind (`conflict` gets `{branch}`,
`{target}`, `{worktree}`, `{paths}`, and `{submodule_conflicts}`;
`gate_failure` swaps the conflict fields for `{log}`) — a typo is a config
error, never a blank sent to the agent.
A repo can carry its own via `[project.<slug>.merge_queue.prompts]`,
merged key by key with your global ones.

If you replace a prompt, keep its rules: the agent must commit on the
branch and must **not** push or touch the target. thegn does the fold and
ref-advance itself, which is what keeps the object store coherent.

## The gate

The folded tip is checked out into a **bare** gate worktree — a fresh
checkout of the merge commit with no dependencies installed. That is the
point (it tests the union, which exists nowhere else), but it means
`gate_command` cannot rely on a project-local toolchain: no
`node_modules`, no virtualenv, no `.direnv`. Rust works out of the box
only because `cargo` is global.

Gate setup, gate and regenerate commands, custom merge drivers and rerere,
Tests panel runs, and command-based test discovery execute candidate code on
the host without isolation, with access to its environment, files, and network.
`thegn doctor` lists these under “Host execution (no isolation)”; a non-empty
failed or errored gate log starts with a line identifying host execution.

Provision it with `gate_setup_command`, which runs first and is **not**
part of the verdict:

```toml
[merge_queue]
gate_setup_command = "pnpm install --frozen-lockfile"
gate_command = "pnpm test"
gate_setup_timeout_secs = 1800
gate_timeout_secs = 3600
```

`gate_setup_timeout_secs` defaults to `1800` seconds (30 minutes), while
`gate_timeout_secs` defaults to `3600` seconds. Set either to `0` to disable its
deadline, or tune setup and gate separately. A timeout is an infrastructure hold
(`gate_error`), never a red result attributed to the candidate branch. Filesystem
operations around workspace preparation cannot be interrupted by these command
deadlines, and a process stuck in uninterruptible OS I/O can delay reaping and
keep its quarantined lease held. The matching environment overrides are
`THEGN_MERGE_QUEUE_GATE_SETUP_TIMEOUT_SECS` and
`THEGN_MERGE_QUEUE_GATE_TIMEOUT_SECS`.

Pressing Ctrl-C or sending SIGTERM during `thegn integrate`, `thegn land`, or
`thegn merge drain` cancels the fold without blaming a branch. If cancellation
stops a gate already running for a row, that row is recorded as `gate_error`;
rows not yet started remain queued.
The gate also returns `gate_error` if its command moves checkout `HEAD` or changes
the index from the pinned candidate.

When a red union is being localized, `diagnosis_max_gates` (default `8`) caps
the total gate attempts and `diagnosis_timeout_secs` (default `3600`) bounds
each setup and gate phase by the diagnosis time remaining when that prefix
starts. Setup and gate deadlines for a prefix are calculated together, so the
phase deadlines can outlast the aggregate diagnosis deadline when setup uses
most of the remaining time. Diagnosis stops without blaming a candidate when
either cap is reached. Set either value to `0` to disable diagnosis and hold the
batch without localization. Increase the caps for large repositories whose
failures need more prefixes or longer setup and test runs.

That split matters, because a gate can fail two different ways and only
one of them is about your branch:

- **`gate_failed`** — the gate ran and went red. A verdict about the
  code, and the one a fixing agent can act on.
- **`gate_error`** — the gate could not run at all (missing binary,
  setup failed, killed). An environment fact. It never wakes the agent,
  never triggers a bisect, and never blames a branch — but it is retried
  on the next drain, since the environment may be fixed by then. The human
  footer and `thegn merge drain --json` report each gate error's `phase`
  (preparation, setup, or gate) and `class` (timeout, spawn, killed,
  setup_failed, capture, poisoned, preparation, signing, source_moved, or
  cancelled). The JSON `gate_verdicts` entries carry both fields.
  `thegn integrate` and `thegn land` include the same `(phase/class)` pair in
  their gate-error messages.

Before each candidate, a reused gate worktree is cleaned of untracked files;
ignored files such as build output remain as a warm cache. Its tracked files are
hashed after setup and checked again after the gate. A changed tracked file
makes the gate an error, not a pass.

## Driving it from an agent pane

thegn ships the queue's instructions to whatever coding agent you run in a
pane, so you don't hand-install anything. When the queue is enabled, each
worktree is seeded with three Claude-format assets:

- `.claude/skills/mq/SKILL.md` — `/mq`, the overview an agent finds on its
  own when you ask it to queue or check a branch.
- `.claude/commands/mq-add.md` — `/mq-add`, commit this worktree's branch
  and enqueue it.
- `.claude/commands/mq-drain.md` — `/mq-drain`, work the queue by hand:
  land what is clean, resolve the rest yourself, repeat. The
  agent-autopilot equivalent of `thegn merge drain`, for when you'd rather
  drive than configure `conflict_handoff`.

They are written at worktree-create time and back-filled at startup, and
each is added to the repo's `.git/info/exclude`, so they never show up as
untracked changes. Nothing is seeded while `[merge_queue] enabled = false`.
Edit them freely — they are rewritten on the next launch, so keep local
changes elsewhere.

## Watching

The full sidebar token and optional `mq` widget show queue depth; the _merge_
section lists entries with their gate state. Failures surface as
notifications. Deferred and red-gate holds create inbox-only `queue_held`
notices, while bookkeeping or persistence failures create inbox-only
`queue_needs_human` entries. Set `[notifications] merge_queue_hold_desktop = true`
(`THEGN_NOTIFICATIONS_MERGE_QUEUE_HOLD_DESKTOP`) to include these notices in
desktop notifications, toasts, sound, and push. The CLI `thegn merge drain`
records the same inbox notices. An unread notice for the same row and state
suppresses a repeat, including after a restart.

A stored queue status thegn does not recognise appears as a red hold naming the
raw status and counts as needing human attention; status writers refuse unknown
values.
