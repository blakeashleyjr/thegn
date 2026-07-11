fn main() {
    // These are cargo build script directives - they MUST use println!
    #[allow(clippy::disallowed_macros)]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        println!("cargo:rustc-env=THEGN_BUILD_TIME={now}");

        // In dev, trigger rebuild when justfile or src changes.
        println!("cargo:rerun-if-changed=src");
        println!("cargo:rerun-if-changed=build.rs");
    }
}
