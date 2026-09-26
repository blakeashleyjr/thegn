Primary coordination for this batch

Investigate only until a concrete plan is approved. Coordinate with THE-465 admission budgets; THE-454 changes svc/calendar transport, while this issue should primarily own core calendar span/expansion and the host projection/reminder consumers. Do not claim the entire THE-457 recurrence issue is fixed incidentally.

Trace all dates_in/occurrences/expand_by_date callers. Current due_reminders flattens per-day buckets, duplicating multi-day occurrences and payloads; avoid that amplification with one shared bounded expansion result for both consumers. Month rendering must preserve the last valid view and visibly report overflow; reminders must not interpret overflow as a successful complete empty evaluation. Define checked span behavior at Chrono extrema and midnight-exclusive ends. Recurring long-duration events can begin before the requested window; simply generating starts from window.from misses overlaps. Address that within a bounded strategy, refusing pathological expansion rather than walking centuries. Share occurrence payloads where possible and count the remaining clone/publication cost before allocation. Keep the no-feature scope.

All workers are Luna high. No Cargo builds/tests or broad checks; central validation is primary-owned. No merge, push, issue close, or extra agents.
