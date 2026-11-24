# Egress wire fixtures (v1)

Golden NDJSON lines for the egress control wire, used by
`crates/thegn-core/src/egress/wire.rs` (`fixture_roundtrip`).

Each file is **exactly one line plus a trailing newline** and is compared both
ways:

1. `serde_json::from_str` of the file equals the value built in the test, and
2. `serde_json::to_string` of that value, plus `\n`, is **byte-identical** to the
   file.

So a renamed field, a changed enum spelling or a reordered field fails the test,
which is the point: these files are the wire, not an example of it
(THE-2123 / WS04-P1-07).

| File                      | Shape                                                                             |
| ------------------------- | --------------------------------------------------------------------------------- |
| `challenge.json`          | `ControlEvent::Challenge`, the server's first line (the `hello` MAC nonce)        |
| `hello.json`              | `ControlRequestLine` + `ControlRequest::Hello`                                    |
| `lease.json`              | `ControlRequestLine` + `ControlRequest::Lease(LeaseSpec)` (no credential `value`) |
| `lease_grant.json`        | `LeaseGrantWire`, the `ok` payload of a `lease` reply (carries the token)         |
| `approval_requested.json` | `ControlEvent::ApprovalRequested`                                                 |
| `audit_record.json`       | `AuditRecord`, schema `v: 1`                                                      |

Rules:

- Changes are **additive** within v1: adding a field with a `#[serde(default)]`
  means these files grow the field (serialisation writes every field), so update
  the file and the test value in the same commit.
- A renamed or removed field is a `PROTO_VERSION` bump and a new socket path,
  never an edit to these files.
- No fixture may ever contain a real token or credential value; the values here
  are obviously fake.
