//! Generates the control-plane gRPC bindings (feature `control-grpc`) from
//! `proto/superzej/control/v1/control.proto` via `protox` — a pure-Rust proto
//! compiler, so no `protoc` is required anywhere (nix devshells, the lean
//! sandbox shell, and bare `cargo build` all stay toolchain-free).

fn main() {
    if std::env::var_os("CARGO_FEATURE_CONTROL_GRPC").is_none() {
        return;
    }
    #[expect(
        clippy::disallowed_macros,
        reason = "println IS the cargo build-script protocol; outln! is for the binary"
    )]
    {
        println!("cargo:rerun-if-changed=proto/superzej/control/v1/control.proto");
    }
    let fds = protox::compile(["proto/superzej/control/v1/control.proto"], ["proto"])
        .expect("compile control.proto");
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(fds)
        .expect("generate control gRPC bindings");
}
