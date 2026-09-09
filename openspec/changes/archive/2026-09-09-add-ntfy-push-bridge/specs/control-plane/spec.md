# Control Plane

## ADDED Requirements

### Requirement: A guarded push command inbox maps signed messages to catalog capabilities

thegn SHALL optionally accept phone-initiated commands through a push command
inbox that is **off by default** and hosted by the daemon process (never the
UI event loop; unavailable — with a reason — when the daemon is disabled).
When `[notifications.push.inbox]` is enabled it MUST require a SecretRef
`inbox_secret` and a non-empty capability `allow` list (enabling without
either is a startup configuration error). The daemon subscribes to the
configured command topic; each message is a versioned JSON envelope
(`v, id, ts, cap, params, mac`) and SHALL be executed only when ALL hold: the
HMAC over the canonical envelope verifies against `inbox_secret`; `ts` is
within the freshness window and `id` has not been seen (replay protection);
`cap` is in the `allow` list; `required_scope(cap)` is within the configured
`scopes` ceiling; and the capability is not admin-scoped (admin capabilities
are refused unconditionally, regardless of config). Execution MUST route
through the same capability-catalog dispatch as the control API — the inbox
is a projection of the one catalog, never a second policy table and never a
shell command. Failed verification, replays, and refused capabilities SHALL
be dropped with counters visible to doctor/logs; replies to an optional reply
topic MUST be truncated to a fixed size cap.

#### Scenario: A signed, allowlisted read command executes

- **WHEN** the inbox is enabled with `allow = ["worktree.list"]` and a fresh,
  correctly signed envelope for `worktree.list` arrives on the command topic
- **THEN** the capability executes through the catalog dispatch under the
  configured scope ceiling and its truncated result is published to the reply
  topic (when one is configured)

#### Scenario: Tampered, replayed, or unlisted commands are refused

- **WHEN** an envelope arrives with a bad MAC, a stale timestamp, a
  previously seen id, a capability outside the allow list, or an admin-scoped
  capability
- **THEN** nothing executes; the message is dropped and the corresponding
  refusal counter increments

#### Scenario: Off by default means no subscription exists

- **WHEN** `[notifications.push.inbox]` is absent or `enabled = false`
- **THEN** thegn opens no subscription to any command topic and no
  phone-initiated command path exists

#### Scenario: Enabling without a secret is a configuration error

- **WHEN** the inbox is enabled with no `inbox_secret` (or an empty `allow`
  list)
- **THEN** startup surfaces a configuration error naming the missing key, and
  the inbox does not start
