# THE-710 plan

Scope: derive schemars::JsonSchema over the CalEvent type graph in thegn-core (EventTime, TzRef, EventId, SourceId, EventStatus, Busy, Reminder, Recurrence, RRule, theme::Hue), CalendarIngestBody in thegn-svc.
Approach: no schemars chrono feature (workspace does not enable it; Cargo.lock untouched). chrono fields use field-level `#[schemars(with = "String")]`; RRule uses container-level `with = "String"` matching its manual string serde.
Tests: control_schema guard exclusion removed (UNREPRESENTABLE empty), CalendarIngestBody registered in add! list, snapshot docs/api/control-v1.json regenerated.
Out of scope: any other schema changes.
