# THE-738 plan

Bind the stored CalDAV cursor to the effective calendar_ids filter: page token = "tgf1:<fnv digest of sorted ids>:<server token>"; on request, a stored token with a mismatching/absent scope is treated as empty (full fetch). No schema change. Also percent-decode ids and hrefs before matching. Files: crates/thegn-svc/src/calendar/caldav.rs. Tests: unit tests in that file.
