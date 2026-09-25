# Primary review + greenlight — THE-194

Reviewing row 538's investigation (`.thegn/pipeline/THE-194/maintenance-investigate/538.md`).

**Verdict: APPROVED to implement.** The investigation found the real mismatch and
the primary independently confirmed it.

## Confirmed mechanism

Production at `crates/thegn-host/src/handlers/plugins.rs` does:

```rust
HostVerb::Register => serde_json::from_value::<Contribution>(params)
```

`params` **is** the `Contribution`, flat. But `examples/plugins/hello.sh:12`
sends it nested:

```json
{"method":"register","params":{"plugin":"hello","contribution":{"id":"hello.seg", …}}}
```

`from_value::<Contribution>` on that object fails — the Contribution's own
fields are one level down. So the shipped example cannot register against the
current protocol, exactly as the issue claims, and the golden test hid it by
substituting `neg.accepted_contributions[0]` at `plugin_example.rs:67-69`
instead of parsing what the script actually sent.

## What to implement

1. **`examples/plugins/hello.sh`** — send the `Contribution` flat as `params`.
   Keep `#!/usr/bin/env sh`, keep it ShellCheck-clean, and keep the teaching
   comments accurate (the "stray echo becomes junk" demo is good — preserve it).
2. **`crates/thegn-svc/tests/plugin_example.rs`** — parse
   `msg.params["contribution"]`… no: parse **`msg.params` itself** as the
   `Contribution`, mirroring production, and register that. The negotiated
   manifest contribution must no longer be substituted. After this change the
   script's register params are load-bearing and an invalid example fails the
   test.
3. **Negative contract test** — feed deliberately malformed register params and
   assert a typed diagnostic (the `RpcErrorCode::Invalid` / "bad contribution"
   path), so the fixed test cannot regress into accepting anything.
4. **Drift guard** — the test must execute or byte-for-byte feed the exact
   committed file. It already spawns the real script; keep that and do not
   reintroduce any test-time rewriting of its content.

## Constraints

- Fix the **example**, not the runtime. Do not relax `from_value::<Contribution>`
  to accept the nested shape — the flat shape is the protocol.
- THE-166/THE-162 are unlanded and confirmed non-prerequisites. Do not implement
  manifest enforcement or verb truthfulness here.
- Update the example's comments/docs if the flat shape makes any of them wrong,
  and check `docs/extending/plugin.md` + `openspec/specs/plugin-api` for the same
  nested-shape error; if they are also wrong, fix them — that is the same defect.

## Validation

Do not run cargo/nextest/clippy. The primary runs the batch gate. Record the
focused filters plus the ShellCheck invocation.
