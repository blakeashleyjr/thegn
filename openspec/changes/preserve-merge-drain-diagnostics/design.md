# Exact status fields, separate presentation

The legacy update_merge_status API deliberately treats None as unchanged. Keep
that contract for existing callers. Introduce an additive typed replacement API
whose nullable result_oid, conflict_paths and error_detail fields replace all
three columns in one existing-row UPDATE. None means SQL NULL in this new API.
Require one affected row; preserve queue identity, queued_at and attempt count.

The driver constructs fields according to the actual outcome. Only actual
newline-separated paths occupy conflict_paths; submodule context and ordinary
diagnostics belong in error_detail. Full result OIDs remain separate from
abbreviated human progress text. Each transition clears superseded fields rather
than relying on COALESCE or empty-string pseudo-clears.

Progress carries those same typed fields through the owned host event. The panel
applies them directly instead of guessing from status. Gate-error-only summaries
are failures/holds, never successful "nothing to drain". This does not make a
best-effort driver write atomic with Git, add THE-591-style concurrent row CAS,
or grant a presentation event destructive cleanup authority.

On agent isolation InfraHold, persist/report agent_blocked and one deferred item,
then break that item's attempt loop. Do not spend an attempt, dispatch an agent,
weaken isolation, or retry immediately. The outer selected-item loop keeps its
existing contract. Generic scheduler and sandbox ownership changes are separate.

Tests use private SQLite and owned/injected driver inputs: exact field placement,
NULL versus empty values, conflict-to-error transitions, explicit retry clearing,
identity/attempt preservation, full OID panel equality, truthful summary severity,
and a bounded held-admission path with one attempt and zero agent execution.
Existing authenticated provider-source refusal assertions remain intact.
