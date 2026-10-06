# THE-260 + THE-271 plan

Scope: crates/thegn-host only. monitor_pipeline.rs gets a pure `RosterFence`
(single-flight, generation-ordered, dirty coalescing, failure backoff/log
limiting) and typed `RosterSample` (Ok/Err). action.rs sends typed samples;
run.rs routes through the fence; DispatchRoster carries `stale`; board footer
shows `stale <age>`. No new timers/config/schema. Tests: fence unit tests +
board stale render test. Out of scope: THE-259 retention/query bounds.
