verdict: done
commits: feat(the-88): monitor verbs, report-gated done, /pipeline monitor loop (this commit)
implemented: dispatch report/note/status verbs with validation, bounded status digests, wake-time report/artifact reads, and report-aware verify output
implemented: `{row}` stage binding through fresh, resumed, and daemon-relaunched stage prompts
implemented: bundled `/pipeline` background monitor loop, on-demand `/btw` status, and report/note handoff discipline
verified: just quick thegn-host
verified: cargo nextest run -p thegn-host dispatch (41 passed)
verified: cargo nextest run -p thegn-host -E 'test(mq_assets)' (8 passed, including clap resolution)
verified: cargo nextest run -p thegn-host stage_prompt (1 passed)

## Unverified

- Full `just test`, `just ci`, coverage, cross checks, docs, smoke, and e2e were not run per the scoped dev-loop policy.
- No live daemon or state database was used; wake behavior is covered through the isolated row-read helper test.

findings: status notes retain the newest 20 entries and report text is read only at wake time or explicit status-on-demand.
next: review the single commit and run the pre-push workspace gate when preparing to land.
