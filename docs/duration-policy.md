# Duration policy and refresh limits

Strict numeric configuration supports polling cadences through 31 days and
other durations through ten 365-day years. Values are always in the field's
existing units: seconds, milliseconds, or days. Feature-specific lower limits
and zero behavior remain documented with each key; zero does not acquire a new
global meaning. Absolute timestamps are not subject to duration limits.

`config validate`, config writes, and explicit CLI/profile overlays reject
newly introduced invalid durations. The environment ignores each invalid duration
field while applying valid unrelated overrides, including security policy. Writes
validate before replacing a file. Existing unrelated problems do not prevent
repairing another setting.

The permissive base-file loader retains a successfully parsed configuration and
reports an invalid duration instead of discarding unrelated sandbox policy.
Runtime periodic conversion safely caps an unsupported cadence; destructive
lifetime policy must quarantine an invalid duration rather than shorten it.
This targeted recovery does not change the loader's general parse-error policy.

Scheduler conversion uses ceiling division in a wider integer domain and always
returns a nonzero slot count. Zero/None feature-disable decisions happen before
conversion. A worker panic is reported explicitly through the existing logging
and terminal wake path; no heartbeat timer is added.

Provider time and destructive policy remain a separate THE-484 acceptance gate.
Unknown or invalid creation time must remain visibly quarantined until reconciled;
legacy ledger timestamps must not be retroactively treated as authoritative
create-intent evidence. The numerical boundary alone does not prove safe provider
resource identity or authorize a deletion.
