# THE-88 revision 1 — preserve wait and DB failures

## Gap

`crates/thegn-host/src/cmd/dispatch.rs:922-934` maps every
`ControlClient::wait` error to `(matched=true, gone=true)`. This includes
daemon disconnects, HTTP 401/403/404/5xx responses, malformed responses, and
transport errors. Only a reaped session should become the documented
`gone: true` wake. The current mapping can tell the monitor that a worker
finished when the daemon was unavailable, causing a false verify/advance path.

The wake-time helper at `dispatch.rs:869-884` also maps every `Db::open` or
`get_dispatch` failure to `(None, None)`. The design only permits a genuinely
missing row to produce null report/artifact fields; SQLite I/O/corruption must
remain an error so the monitor cannot mistake an infrastructure failure for a
worker with no report.

## Required correction

- Preserve non-404 control errors through `wait_wake` and return them as
  retryable command failures. Classify only the daemon's not-found response
  for a session that disappeared after selection as `gone: true`.
- Make the wake-time DB helper return `Result<(Option<String>, Option<String>)>`
  (or an equivalent typed distinction): `Ok((None, None))` only for an absent
  dispatch row, while DB open/query errors propagate.
- Add isolated tests for a reaped/not-found wake, a transport/control error,
  and a DB read error, proving only the first emits a successful `gone` wake.

Do not use `--force` or change the report/done gate to hide this distinction.
