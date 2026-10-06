# THE-480 plan

Scope: calendar/tz.rs resolve_local gap branch. Replace offset_from_utc_datetime(local-as-UTC) and the 240-minute scan with gap_transition: bisect the real transition within a +-15h window, shift forward by the pre-transition offset, Earliest = transition minus 1s.
Tests: Berlin, Lord Howe, Apia, NY, bounds no-panic.
Out of scope: proptest over all tzdb transitions.
