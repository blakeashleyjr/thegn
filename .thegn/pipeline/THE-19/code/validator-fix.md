# THE-19 — authorized config_validate union-branch fix (Lead work order)

files:
  - crates/thegn-core/src/config_validate.rs
  - config/config.toml.example

## Authorization

Row 317 correctly escalated instead of papering over the failure, and its
diagnosis is accepted: `example_config_validates_clean` is red because the
**strict schema walker checks every `anyOf` branch of `HookEntry`**, while
serde deliberately accepts BOTH the string and the object form for every hook
event. The walker reports the non-matching branch as an error, so a perfectly
valid example config fails validation.

That is a **defect in the validator, not in the example config or in the
`[hooks]` schema**. Fixing it is authorized by this row.

## Done criteria

- `config_validate`'s schema walk treats a JSON-Schema `anyOf` / `oneOf` union
  as satisfied when **any** branch validates, and only reports an error when
  **no** branch does. The reported message should name the union
  (what shapes were acceptable), not one arbitrary branch's complaint.
- Add regressions in `crates/thegn-core/src/config_validate.rs` covering:
  - a union key given in each accepted form (string, object) — both clean;
  - a union key given in a form no branch accepts — one clear error naming the
    acceptable shapes;
  - a nested union inside a list entry, which is the `[hooks]` shape.
- `cargo nextest run -p thegn-core -E 'test(/config_validate|example_config_validates_clean/)'`
  passes.
- Do NOT weaken strictness elsewhere: a key with a single (non-union) type must
  still be strictly checked exactly as today. If your change makes any existing
  validation more permissive, call that out explicitly in the artifact.
- `config/config.toml.example`'s `[hooks]` examples should demonstrate BOTH
  accepted shapes, since the point is that both are legal.
- Then run the full gate: `THEGN_ALLOW_HEAVY=1 just test`, and report its
  result. If it dies before any test runs (e.g. `sccache: Operation not
  permitted`), retry once with `RUSTC_WRAPPER=` unset and report BLOCKED with
  the exact error rather than FAIL.
