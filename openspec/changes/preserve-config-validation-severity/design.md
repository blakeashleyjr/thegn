# Typed diagnostics at the validation boundary

Normalization already defines accepted aliases and canonical-wins collisions.
Both remain advisory during the compatibility window. Validation wraps those
messages as typed warnings and schema/type/semantic failures as typed errors;
severity is never recovered by matching message text. Existing `validate_str`
callers receive actual errors only, consistent with its documented contract.

Host health renders warnings and counts them separately for main and external
profile TOML; repository overlay validation retains its independent trust rules.
CLI validation succeeds on warnings alone. Config-set rejects only newly
introduced errors and still reports advisory messages. No live config or database
migration is part of this repair.
