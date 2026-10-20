# Preserve configuration validation severity

THE-569 repairs the configuration-admission path tracked under roadmap group O,
item 188 (validation and error surfacing). Accepted compatibility spellings become fatal health
findings because warning strings share the error vector.

Preserve normalization's existing canonical-wins collision policy. Add typed
warning/error diagnostics, keep the historical errors-only validation API, and
carry severity into health counts, CLI status and config-set validation. Do not
rewrite operator configuration or weaken malformed/unknown-value validation.
