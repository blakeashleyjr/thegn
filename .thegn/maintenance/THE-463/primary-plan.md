# Primary review + greenlight — THE-463

Reviewing row 523's investigation (`.thegn/pipeline/THE-463/maintenance-investigate/523.md`).

**Verdict: APPROVED to implement**, points 1-5 exactly as written. Point 6 is
decided below.

The plan is right in the way that matters: **one** account-policy wrapper around
every backend, not a policy check inside each provider. The typed
`CalendarError::ReadOnly(CalendarMutation)` kept distinct from `Unsupported` is
exactly the distinction this issue exists to create — "the provider cannot" and
"policy refused" must never collapse into one error.

Two findings are adopted and worth restating so they are not lost in
implementation:

- **`calendar.ingest` is cache ingest, not upstream mutation.** The gate must
  NOT block it. Blocking ingest would break read-only accounts entirely, which
  is the opposite of the intent.
- **`CalendarRouter` currently drops account policy when building adapters.**
  That is the actual hole; wrapping in `from_config_with_pool` closes it.

## Decision on point 6 — narrow the raw constructors to `pub(crate)`

Make the invariant **structural, not documentary**. A comment saying "raw
adapters are provider-only" is a convention that the next contributor breaks;
`pub(crate)` is a compiler error. Expose only the account-bound factory outside
the crate.

The primary checked the blast radius. Raw `*Backend::new` constructors are
`pub` in `command.rs:337`, `ics.rs:41`, `ics_url.rs:48`, `caldav.rs:50`, and
there is exactly **one** out-of-crate consumer:

- `crates/thegn-svc/tests/calendar_plugin_alloc.rs:82` — `command::CommandBackend::new(...)`

Adjust that test to build through the account-bound factory (or move it into the
crate as a unit test, if the factory makes the allocation assertion awkward).
One integration-test adjustment is a fair price for a bypass that cannot be
reintroduced by accident. `crates/thegn-svc/src/conformance.rs:234` already goes
through `calendar::backend_from_account`, which is in-crate, so it is unaffected.

If narrowing turns out to break something the primary did not see, STOP and
report it rather than falling back to the documentation-only option silently.

## Restated constraints

- Capability intersection is `provider_bit && !read_only` for create/update/
  delete only. `incremental` and `server_expand` pass through untouched.
- Diagnostics: bounded `tracing` on the existing calendar target with account,
  provider and operation fields. **No URLs, tokens, event payloads or command
  arguments.** No new audit store.
- No new HTTP/CLI/MCP route in this lane. The consumed-config-field ratchet
  stays a recommended follow-up, out of scope — note it, do not build it.
- THE-166/THE-162 are unlanded: preserve today's command-plugin negotiation
  as-is. The wrapper surrounds the command adapter so a future plugin cannot
  out-vote account policy; that is the whole plugin requirement here.

## Tests required

The regression matrix in the plan is approved. It must include the
**contradictory case**: a provider spy advertising write support on a
`read_only = true` account — policy wins, the spy is never called, and the
error is `ReadOnly`, not `Unsupported`. Also assert ingest still works on a
read-only account.

## Validation

Do not run cargo/nextest/clippy. Record the focused filters. Changing a public
error enum or the svc surface may move `docs/api/control-v1.json`
(`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema`) —
flag it in your report; the primary regenerates centrally.

---

## Blocker resolution — `calendar.ingest` (primary, after row 527)

Row 527 reported a concrete blocker: the primary's test requirement "assert
ingest still works on a read-only account" cannot be satisfied, because
`calendar.ingest` exists only as the `ControlApi` `Unimplemented` default in
this checkout. Implementing it to make the assertion possible would be an
out-of-scope feature.

**The coder was right to stop rather than build it. The requirement is
withdrawn and replaced.**

Replacement requirement (already satisfied by the committed wrapper): the
policy boundary must be provably confined to the three mutation verbs. That is
asserted structurally — `AccountPolicyBackend` delegates `list_events` and
every non-mutation method to the inner backend unchanged, and `caps_for_policy`
clears only `create`/`update`/`delete`, leaving `incremental` and
`server_expand` untouched. A future `calendar.ingest` therefore cannot be
caught by this gate, because the gate has no hook on any read path.

The original intent — "a read-only account must still be able to read and
refresh its cache" — is preserved by that confinement. When `calendar.ingest`
is genuinely implemented, its lane owns the test.

Recorded as a follow-up, not a defect in this change.
