# Bound duration and refresh policy

THE-483 identifies overflow in the shared refresh ticker's duration-to-slot
conversion. THE-484 identifies signed duration narrowing that can invert cache,
lease, and billable-resource expiry policy. Syntactically valid configuration,
programmatic values, and untrusted provider delays need safe runtime arithmetic
in both debug and release builds.

Use one unit-aware checked time boundary, document supported numeric ranges in
the config schema, reject newly invalid explicit overrides and writes before
publication, and retain successfully parsed base-file security configuration
while diagnosing and safely handling an invalid duration. Consumer safety remains
mandatory when validation is bypassed. Destructive reaper policy must quarantine
unknown time/invalid lifetime rather than infer expiry.

Impact: existing configuration layering, background refresh scheduling, and
provider lifetime/freshness policy. This repairs roadmap A.12/A.15 configuration
and background workers and item 412 monitoring continuity; no new feature or
control surface is introduced. THE-483 and THE-484 remain separate Linear
acceptance contracts coordinated by THE-614; delivery registration belongs to
the integrating reviewer.
