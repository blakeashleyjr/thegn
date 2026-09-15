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
