# Design

## CI bootstrap artifacts

Keep release, exact byte size, SHA-256, byte/time limits, and allowed origins in
reviewed source. Use explicit release URLs, stage downloads in private
directories, verify exact size and digest, and only then execute the Nix
installer or promote the runner archive. Re-authenticate a cached runner
archive on every run. Before extraction, reject unsafe member paths, escaping
links, unsupported file types, duplicate destinations, excessive member counts,
and excessive expanded size. Extract to a private staging directory and promote
the completed payload atomically.

## Bare macOS contributor bootstrap

Use the Rust project's versioned `rustup-init` archive for each supported Mac
architecture rather than the mutable `sh.rustup.rs` installer. Pin the exact
size and SHA-256, download from the approved Rust archive host, and verify bytes
before execution. Rustup continues to verify signed channel metadata for the
stable toolchain.

## Pin refresh

Select an exact release tag or tool version, record its byte size and SHA-256,
and cross-check publisher metadata when available. Update version, size, digest,
limits, and any needed origin allowlist together in a reviewed change. No setup
path resolves a mutable `latest` endpoint.
