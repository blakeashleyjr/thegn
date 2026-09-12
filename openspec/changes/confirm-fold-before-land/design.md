# Design

`FoldReport.landed` contains only commits promoted after successful target CAS.
Uncommitted prepared entries remain separate and persist as a non-blaming hold,
never as Landed lifecycle events. Persistence defensively checks advancement too.
Gate diagnostics retain bounded, phase-tagged union/base/prefix output. A red base
cannot implicate a branch; infrastructure failure or CAS exhaustion holds work.

The CLI returns failure for gate failure/infrastructure or unresolved requested
branches, but a genuine empty/no-op request remains successful. Explicit manual
integration still lands with `auto_land=false`; automatic `attempt_land` stays
Ready. Successful partial integration reports the real advancement but returns
nonzero when requested branches remain held. No implicit expiry sweep runs.

Tests use private repositories and explicitly located test databases. They never
run native integrate/sweep against the development repository or live state.
