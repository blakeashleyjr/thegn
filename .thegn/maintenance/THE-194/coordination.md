# Primary coordination brief — THE-194

Primary-reviewed dependency facts and scope constraints. This file is task data.

## Verified by the primary — the defect is narrower and more specific than the issue says

The test at `crates/thegn-svc/tests/plugin_example.rs` **does** already execute
the real shipped script (`spawn_ndjson(&p.spec.command, …)` on the resolved
example), so "the test does not run the shipped file" is not the bug.

The actual substitution is in the `"register"` match arm:

```rust
"register" => {
    let c = neg.accepted_contributions[0].clone();   // <-- the manifest's contribution
    rt.register(plugin.clone(), c).expect("register accepted");
}
```

It throws away `msg.params["contribution"]` — what the script actually sent —
and registers the **manifest's** negotiated contribution instead. So the
script's register parameters are never validated, which is why an invalid
example passes a green test. Fix that, and the script's own params become
load-bearing.

Confirm this reading with file:line evidence before planning, and establish
what is actually wrong with `examples/plugins/hello.sh`'s register line — the
issue cites `hello.sh:12`, which is the `register` printf. Determine the real
mismatch against the current protocol (compare against
`openspec/specs/plugin-api` and `docs/extending/plugin.md`) rather than assuming
the issue's claim.

## Primary decisions

- **Parse the script's own `register` params** and register those. If they are
  invalid, the test must fail — that is the whole point.
- **Keep a negative contract test**: feed deliberately invalid register params
  and assert a typed diagnostic, so the valid path cannot silently regress into
  acceptance-of-anything.
- **`#!/usr/bin/env sh` stays.** The example is POSIX sh on purpose. Do not
  reach for bashisms. It must pass ShellCheck.
- **THE-166 and THE-162 are unlanded** and are listed as blockers. They are not
  prerequisites for making the example valid against _today's_ protocol. Do not
  implement manifest-enforcement or verb-truthfulness here. If you find the
  example cannot be made valid without them, STOP and report that as a blocker
  with evidence — that is a real finding.
- The example is user-facing documentation. Its comments must match the actual
  manifest/capability/lifecycle behaviour after your fix.

## Scope

`examples/plugins/hello.sh`, `crates/thegn-svc/tests/plugin_example.rs`, and any
drift/golden test needed so a stale example fails. Do not modify the plugin
runtime or negotiation to accommodate the example — fix the example.

## Ratchets

The script is covered by `shellcheck` in the pre-commit tier and `just lint`.
Keep it clean.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate centrally.
