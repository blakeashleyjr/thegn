verdict: done
commits: 1146ddda feat(the-88): dispatch report column + per-row progress queue (v61)
implemented: schema v61 report column + progress-note table/index and v60 ladder migration
implemented: AgentDispatch.report, DispatchNote, SQLite report/note APIs, pure report validation/digest
implemented: report-gated verify facts, {row} stage variable, CLI-only capability/completion rows
verified: just quick thegn-core
verified: nextest filters pipeline_report, pipeline_run, db_migrate, capability, stage_vars agent_task, dispatch, completion::catalog, config_validate, ratchet
verified: pre-commit treefmt/shellcheck/yamllint
unverified: full just test, just ci, coverage, cross checks, docs, smoke/e2e (not run per scoped policy)
findings: chunk-2 must extend roster reads/CLI JSON and bind the actual row id when rendering stage prompts
next: consume db_dispatch + pipeline_report; preserve report cap and separate daemon note ledger
