# Primary review + greenlight — THE-574

Reviewing row 528's investigation (`.thegn/pipeline/THE-574/maintenance-investigate/528.md`).

**Verdict: APPROVED to implement**, points 1-4 as written, with one required
addition.

The plan's most important judgement is correct and must be preserved: only the
error from `launch_spec_synced_with` becomes observable. The other `None`
returns in the ladder — DB open, missing `worktrees.agent` row, `shell` /
`clean-shell` / tool-drawer exclusion, and the unconfigured-entry `?` — stay
silent. Those are ordinary "not applicable" outcomes, not refusals, and making
them noisy would be a regression.

Choosing `tracing` over a new channel is also right: it sidesteps the
channel-plus-waker contract entirely for what is a diagnostic, and keeps this
lane small.

## Required addition — the level must be WARN, and here is why

Per CLAUDE.md, **no logging sink is installed when `THEGN_LOG` is unset** — no
file, no stderr layer. What _is_ always on is a minimal diagnostics layer
holding a fixed-size in-memory **WARN+** ring (`thegn_core::diagnostics`),
reused for crash reports and the debug bundle.

So the level is not a style choice: at `debug!` or `info!` this refusal is
discarded entirely in a default run and the issue is not actually fixed. Emit it
at **`warn!`**, so it lands in the always-on ring and reaches `thegn doctor` /
the debug bundle with no environment variable set. State this reasoning in a
code comment so nobody "tidies" it down to `info!` later.

## Scope note — UI surfacing is a follow-up, not this lane

A `warn!` makes the reason _recoverable_ (debug bundle, `THEGN_LOG=info`) but
not _immediately visible_ to a user staring at an agent-attributed tab holding a
shell. That is acceptable here — the issue explicitly permits the "worker-to-UI/
**log** diagnostic seam" — and the alternative (a `model.status` toast from a
`spawn_blocking` worker) needs the channel-plus-waker path this lane is
deliberately avoiding.

**Record it as an explicit follow-up finding in your artifact** rather than
silently doing it or silently omitting it.

## Restated constraints

- Fail-open behaviour is unchanged: a refusal still degrades to the
  already-resolved shell, never to a failed tab.
- `suppress_agent_record: true` stays. Do not clear or rewrite `worktrees.agent`.
- Do not touch `attach_is_empty`, `quiet_split`, terminal gating, shell
  materialization, or daemon session adoption. Do not move work onto the loop.
- Dedupe key `(worktree, agent, refusal-kind)` for the process lifetime, with a
  generic provider/launch bucket so THE-565 can add categories later without
  another rewrite. A process-local registry is fine; no DB I/O, no new
  persistent state.

## Ratchets

You are **removing** a `.ok()`, which is good. Any new `let _ =` / `.ok()` you
add needs a `// best-effort: <why>` comment or the ignored-result ratchet fails.
Do not add a poll on the idle loop.

## Tests

The failure-case table in the plan is approved. The four assertions per case
stand: shell fallback still happens, no agent command executed, the reason is
observable, and a repeat call does not repeat the report.

## Validation

Do not run cargo/nextest/clippy. The primary runs the batch gate centrally.
