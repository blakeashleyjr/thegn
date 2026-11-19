fn main() {
    #[expect(clippy::disallowed_macros, reason = "Cargo build script directive")]
    {
        println!("cargo:rerun-if-changed=proto/guest.proto");
    }
    let descriptor =
        protox::compile(["proto/guest.proto"], ["proto"]).expect("compile guest.proto");
    prost_build::Config::new()
        .compile_fds(descriptor)
        .expect("generate guest wire types");
}
