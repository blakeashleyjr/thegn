fn main() {
    // These are cargo build script directives - they MUST use println!; the git
    // sha probe runs at build time (not on the event loop), so the disallowed
    // `Command::output` lint does not apply here.
    #[allow(clippy::disallowed_macros, clippy::disallowed_methods)]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        println!("cargo:rustc-env=THEGN_BUILD_TIME={now}");

        // Best-effort short git sha for crash reports / doctor identification.
        // Absent (empty) when git or the repo is unavailable (e.g. a source
        // tarball build) — the report then records just the version.
        let sha = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        println!("cargo:rustc-env=THEGN_GIT_SHA={sha}");

        // In dev, trigger rebuild when justfile or src changes.
        println!("cargo:rerun-if-changed=src");
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-changed=../../.git/HEAD");
    }
}
