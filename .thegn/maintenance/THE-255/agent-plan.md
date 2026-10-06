# THE-255 / THE-256 / THE-261 plan (one lane, one commit each)

All validation in thegn-core, pure, unit-tested.

- THE-255: pipeline_chunk::parse_frontmatter tracks seen-key flags independent of values. Tests: all shape pairs x 3 keys, single-empty legal, unknown keys repeat.
- THE-256: validate_pipeline rejects timeout_secs == 0 and > time_policy::MAX_DURATION_SECS (so *1000 fits i64); add PipelineStage::wait_timeout_millis() checked conversion; fix docs (dispatch wait, ms) and SKILL.md. Board keeps tolerating 0 for invalid configs.
- THE-261: stage name grammar [A-Za-z0-9][A-Za-z0-9._-]*, <=64 bytes, no trailing '.', no raw whitespace/control (untrimmed name must equal trimmed), reserved "unstaged", case-insensitive uniqueness (artifact dirs). Accepts maintenance-* names. Also applied to `next`? next must name a configured stage so it is covered. Out of scope: a typed StageName across host paths (validated config guarantees canonical form).
