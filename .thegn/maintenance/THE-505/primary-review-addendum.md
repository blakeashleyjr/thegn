Primary review addendum while row 487 was active

Native workers run codex exec; terminal send is not reliable steering. This records concrete findings that must be addressed before approval, or handed off explicitly pending.

An exact standalone snapshot of config_budget.rs (sha256 e30e45b298c1ee628c067b3f17b99cfce791c18ecce02771ab6eb14ca8e0d47e) produced:

- Separate valid headers, each depth 41, incorrectly Err(Depth): format!("[{}a]\nx=1\n[{}b]\ny=1\n", "p.".repeat(40), "q.".repeat(40)). Absolute table headers must reset prior table depth before counting.
- Table depth 63 plus 10 nested arrays incorrectly Ok: format!("[{}z]\nx={}1{}\n", "a.".repeat(62), "[".repeat(10), "]".repeat(10)). Charge inherited path depth at every value container opener.
- Dotted outer key plus nested dotted inline table incorrectly Ok: format!("{}v = {{ {}w = 1 }}\n", "a.".repeat(40), "b.".repeat(40)). Preserve outer key path depth when entering inline table; restore proper parent scope on exit.

Probe source and output are in /home/blake/code/thegn/target/maintenance-review-20260917/next-native-batch/config_budget_scope_probe.rs and config-budget-scope-probe.txt. Add valid cap and cap+1 regressions, including quoted path components and sibling scopes. Arrays member overflow now correctly fails in this snapshot. No whole-crate compile/test result is implied.

Additional source finding: config_compat::normalize_admission now uses String::with_capacity(MAX_NORMALIZED_BYTES) and then an unrestricted TOML Serializer into it. Initial capacity is not a maximum; String grows beyond it. This does not satisfy the requested pre-serialization bound. Prove an encoded-size bound before serialization (including key-path repetition and escaping) or use an actually bounded/counting serializer path; do not claim that with_capacity alone limits memory. Avoid allocating 8 MiB for every small config as an incidental performance regression.

Review of encoded_upper_bound: multiplying each key by 128 does not prove a table-header bound. A parent key is repeated for every descendant table, not only once per nesting depth. E.g. a 5500-byte parent header followed by 1500 short child assignments `k0 = {}` produces many repeated long parent paths from compact valid input. Compute the sum of each emitted full table path (including array-of-table paths) using inherited path length and actual descendant count, with saturating/checked budget rejection, rather than a depth-based repetition multiplier. Test output length <= predicted bound on adversarial wide/deep trees, and reject before serializing when the predicted output exceeds the budget. A capacity check after serialization is only a backstop, not the allocation boundary.
