# THE-462 plan

Implement calendar_ids filtering in CalDAV: filter multistatus responses (events and tombstones) by parent-collection href match, segment aligned, empty = all. Files: thegn-svc/src/calendar/caldav.rs, doc comments. Tests: pure multistatus unit tests. Out of scope: collection discovery, token invalidation on filter change.
