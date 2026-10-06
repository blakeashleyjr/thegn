# THE-466 plan

Choice: remove dead API, not make authoritative (would be new feature, conflicts with THE-463 tests pinning backend-reported Unsupported).

- Remove CalendarCaps.server_expand (nothing set or read it) and .incremental (never consulted; token handling is carried by the page, THE-452).
- Keep create/update/delete: policy-masked (THE-463) and published via CalendarRouter::caps.
- Update backends and tests; no behaviour change.
