# THE-267 plan

Scope: daemon reaper only. Files: thegn-core db_dispatch.rs (new run-fenced status CAS), thegn-host daemon/pipeline_reaper.rs.
Approach: capture (id, session, run_gen) with the roster snapshot; before each park/close re-read daemon liveness (None => no-op, session live => no-op); CAS on status + session + run_gen.
Tests: core CAS fence test; reaper tests for late-opened session, unavailable liveness, relaunch between plan and apply.
Out of scope: pipeline_retry.rs, schema, CLI reap.
