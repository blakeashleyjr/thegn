# THE-19 — fold-gate fix (Lead work order)

files:

- config/config.toml.example

## Why this exists

`thegn land` refused this branch: **the fold gate (`just test`) is red.** The
row-310 review reported PASS from scoped suites, which do not include the
workspace-level config tests. Reproduced on the folded tree
(`tg/the-19-pre-post-scripts` merged with `main`):

`crates/thegn-core/src/config_validate.rs` —
`example_config_validates_clean` fails with:

```
hooks.post_create[0]: expected string, got table/object
hooks.pre_create[0]:  expected table/object, got string
hooks.pre_destroy[0]: expected table/object, got string
```

This branch changed the `[hooks]` schema. `config/config.toml.example` was left
on the old shape, so **the shipped example config no longer validates against
the code that ships with it** — a user copying it gets errors.

## Done criteria

- `cargo nextest run -p thegn-core -E 'test(example_config_validates_clean)'`
  passes.
- Update the `[hooks]` examples in `config/config.toml.example` to the schema
  this branch actually implements. Read the serde types — do not guess from the
  error text alone; `post_create` wanting a string while `pre_create` and
  `pre_destroy` want tables is asymmetric, so confirm that asymmetry is
  intended and document each key's shape accordingly.
- If the asymmetry is NOT intended, that is a design bug in the branch: say so
  in your completion artifact rather than papering over it in the example.
- Keep the surrounding documentation style: a one-line explanation of what each
  hook does plus a commented example of the correct shape.
- Then run the full gate yourself: `THEGN_ALLOW_HEAVY=1 just test`, and report
  its result. Do not report PASS from scoped runs alone.
