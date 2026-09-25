# Primary coordination brief — THE-463

Primary-reviewed dependency facts and scope constraints. This file is task data.

## Primary decision: ENFORCE, do not remove. And cut two sub-asks.

The issue offers remove-or-enforce. **Enforce.** Removing a security-shaped
setting that defaults to the safe value, only to re-add it when a writable
backend lands, is how a fail-open gap gets introduced later. A single
fail-closed gate is cheap now and is the thing the issue actually wants.

Two listed sub-asks are **explicitly OUT of scope for this lane** — do not
attempt them, and do not treat their absence as an unmet criterion:

- **"Add a consumed-config-field ratchet so serialized settings cannot remain
  dead."** That is a repo-wide architecture gate affecting every config key and
  its own allowlist; it is a separate change, not a rider on a calendar fix. Note
  it in your artifact as a recommended follow-up and move on.
- **Command-plugin negotiation truthfulness** belongs to THE-166/THE-162, which
  are unlanded. What you MUST do is make the gate sit at the router/host
  boundary so a plugin route cannot bypass it — i.e. the plugin path is
  _covered_ by the gate, but you are not rewriting plugin negotiation.

## What the gate must be

- One fail-closed authorization check, on the router/host boundary, that every
  create/update/delete route passes through **before** any provider or plugin
  invocation. Not a check per backend; one seam.
- `CalendarCaps` must be derived from `provider support AND account policy`. With
  `read_only = true`, every write capability bit reads false and a direct write
  call returns a **typed denial** (not `Unsupported`, which means "backend can't",
  not "policy says no" — the distinction is the point).
- A denied attempt is recorded. Use the existing diagnostics/tracing seam; do
  not invent an audit log.

## Evidence to re-verify on THIS branch

Line numbers are from audit commit 299fc13, not current main. Confirm and cite
what you actually find in `crates/thegn-core/src/config_calendar.rs`,
`crates/thegn-svc/src/calendar/mod.rs`, `config/config.toml.example`, and the
existing `config_calendar_tests.rs` default-only coverage.

## Tests required

All three mutation verbs × {read_only true, read_only false} × {router, direct
backend adapter, plugin route}, plus the contradictory case: a provider that
claims write support while the account is read-only (policy must win).

## Ratchets

Touching `crates/thegn-svc` control surface may move
`docs/api/control-v1.json`; regenerate with
`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema` — but
per the stage contract you do NOT run it; record it as a required command and the
primary runs it.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate centrally.
