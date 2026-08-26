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
        // Scrub the repo-targeting env by hand (the `util::git_cmd` seam is in
        // thegn-core, which a build script cannot depend on): building from
        // inside a git hook — the merge-queue fold gate does exactly that —
        // exports GIT_DIR/GIT_WORK_TREE and would stamp the OUTER repo's sha.
        let mut probe = std::process::Command::new("git");
        for var in [
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            probe.env_remove(var);
        }
        let sha = probe
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
