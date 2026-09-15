# Reviewed verification — 2026-09-15

The final source `72d161b2e1868561fd2f5bce9facde8b0345512a` passed strict
workspace all-target Clippy and the shared 145-test Linux matrix (77 core,
16 svc, 52 host). See [the durable audit](../../../docs/audits/maintenance07-2026-09-15.md)
and its hashed manifest for exact source, selectors, raw logs, historical
failures, independent reviews, and platform/scope limitations. No additional
release or per-issue compilation was used.

## Historical source checkpoint

# THE-326 source checkpoint — 2026-09-15

The private candidate implements the typed serializer/parser boundary in
`crates/thegn-svc/src/share/mod.rs`, updates the pure FRP planner tests in
`crates/thegn-svc/src/share/tests.rs`, and documents the remote-port policy in
`crates/thegn-core/src/config.rs`. The source review specifically checks that
the generated document uses `deny_unknown_fields` at root/auth/proxy levels,
that `remote_port = 0` is refused before serialization, and that no raw
`toml::Value` parser or handwritten TOML escaping remains.

The candidate has only received source formatting and `git diff --check` in
this review. Cargo, native, provider, and runtime gates remain pending root
review; this checkpoint makes no test-pass or landing claim.
