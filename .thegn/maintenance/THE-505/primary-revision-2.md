# Primary review of eed61593: bounded foundation revision 2

The earlier process-global installation defect is repaired by source inspection. The current foundation still does not compile and its scanner admits several valid over-limit inputs. Repair these concrete defects before host wiring. Remain in chunks 1-2.

## Executed narrow reproductions

Primary extracted the unchanged config_budget scanner/check_entries prefix (excluding only TOML-dependent decoded walk) and compiled it with rustc. It fails E0282 at containers.last().is_some_and before Rust infers Vec<Container>. Declare that vector type explicitly. After adding ONLY that annotation in a temporary copy, the following all returned Ok even though they exceed the advertised limits:

```rust
let inherited = format!("[{}z]\n{}v = 1\n", "a.".repeat(62), "b.".repeat(62));
// Actual combined table/key depth exceeds 64.
let physical = format!("x = \"\"\"{}\"\"\"", "a".repeat(MAX_LINE_BYTES - 6));
// Actual line is 16388 bytes against 16384 cap: triple delimiters skip bytes.
let members = format!("x = [\n{}1\n]\n", "1,\n".repeat(MAX_MEMBERS));
// 16385 values against 16384 cap: counting commas misses first member.
```

Reproductions are real source probes, not a crate test pass. Add exact regressions plus equal-limit success cases. Count all physical bytes in a separate bounded pass or an equivalent correct mechanism. Accumulate inherited table path, dotted key wrappers, and inline-container depth; quoted dots are not separators. Count array values correctly with and without trailing commas. Preserve multiline 4/5 closing-quote TOML forms and comments. The current 'table inheritance' test only tests shallow input and does not cover the defect.

You MAY compile an extracted dependency-free scanner snapshot with rustc --emit metadata or a tiny standalone probe to catch syntax/type errors; this explicit exception is cheap and does not authorize Cargo/build/nextest/Clippy or full suites. Primary still runs the crate checks centrally.

## Remaining boundary corrections

- post_process_pure still calls util::expand_tilde, which reads ambient HOME. Make normalization consume a captured home/path-expansion context carried by AdmissionInputs and included in private revision material; do not reopen process environment during admission. Keep legacy effectful wrapper behavior through its own captured context. Test two admitted candidates using the same captured inputs remain identical while an injected unrelated ambient source would change; avoid unsafe process-global env mutation.
- Admission still serializes arbitrary defaults/current cfg through apply_json_overlay/apply_override_str/check_final_config before its bounded output checks. Reuse/refactor host_config_checked borrowed recursive preflight and bounded writer BEFORE first recursive conversion/clone. Existing strict final 4MiB/depth32/container1024 limits remain authoritative. Validate every source/candidate transition before expansion, not just after the resulting Config exists. Avoid unrestricted serde_json::to_value on an unadmitted Config.
- bounded_json_bytes controls length but Vec growth can exceed limit. Use a reviewed bounded capacity strategy. Vec::try_reserve_exact argument is additional relative to len, NOT capacity; len6/cap8/add4/limit10 must never become cap16. Prefer bounded geometric growth to avoid per-chunk quadratic copies. Apply the same reasoning to compatibility TOML serialization, which currently to_string()s before checking size. A counting pass or bounded serializer adapter is acceptable; document peak retained bounds honestly.
- Do not depend on warning prose or incidental warnings for strict CLI schema. Validate the supplied raw key/value shape/enums before serde can warn-and-default; share mapping with legacy behavior, rather than duplicate allowed keys. Include bad enum, numeric range, unknown leaf, deeply nested value, quoted CLI strings, arrays and valid custom map keys. New source identities/context strings themselves need bounded admission before hashing/copying.
- AdmissionStore currently uses independent current/health locks, permitting inconsistent reads and failure/publication races. This live-store API is outside chunks1-2. Preferred: remove/defer AdmissionStore/RevisionGuard/require_current to chunk6 and retain only immutable ConfigRevision vocabulary here. Record the required later single atomic state, failure epoch, CAS and operation-authority window in the handoff. Do not call a detached revision snapshot a real guard. If retained, unify and prove race semantics now instead of leaving a known wrong API for host wiring.

Add meaningful regression tests for each fixed invariant. Run allowed small source checks, commit, and report implementation-ready with full Rust tests still pending primary. Do not move to host/provider chunks or claim issue completion.
