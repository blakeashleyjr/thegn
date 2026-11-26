use std::path::{Path, PathBuf};

// A hook may export repository selectors for a different checkout. Every
// metadata query must use the same scrubbed environment as the identity query.
#[allow(clippy::disallowed_methods)]
fn git(args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    let output = command.args(args).output().ok()?;
    output.status.success().then_some(())?;
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn git_path(name: &str) -> Option<PathBuf> {
    git(&["rev-parse", "--path-format=absolute", "--git-path", name]).map(PathBuf::from)
}

#[allow(clippy::disallowed_macros)]
fn watch(path: &Path) {
    // Cargo treats a permanently missing watch as dirty on every invocation.
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn watch_git_identity() {
    if let Some(head) = git_path("HEAD") {
        watch(&head);
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git_path(&reference)
    {
        // A packed ref has no loose file. Watch the nearest existing refs
        // directory for its creation, without watching objects/logs/index.
        let mut existing = path.as_path();
        while !existing.exists() {
            let Some(parent) = existing.parent() else {
                break;
            };
            existing = parent;
        }
        watch(existing);
    }
    if let Some(packed) = git_path("packed-refs") {
        watch(&packed);
    }
}

fn main() {
    // `voice_native`: the `voice-native` feature AND a target that can link the
    // native engine (Linux glibc, macOS). On musl static and Windows builds the
    // feature is a no-op and voice reports "not built into this binary". Keep
    // the predicate in sync with crates/thegn-voice.
    #[allow(clippy::disallowed_macros)]
    {
        println!("cargo:rustc-check-cfg=cfg(voice_native)");
        println!("cargo:rustc-check-cfg=cfg(voice_metal)");
        let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        let supported = os == "macos" || (os == "linux" && env == "gnu");
        if std::env::var_os("CARGO_FEATURE_VOICE_NATIVE").is_some() && supported {
            println!("cargo:rustc-cfg=voice_native");
            // thegn-voice links whisper.cpp with Metal on every macOS build and
            // uses the GPU (CPU fallback), so `voice_metal` follows the target.
            if os == "macos" {
                println!("cargo:rustc-cfg=voice_metal");
            }
        }
    }
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
        let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        println!("cargo:rustc-env=THEGN_GIT_SHA={sha}");

        // Resolve Git paths for both ordinary and linked worktrees (THE-575).
        println!("cargo:rerun-if-changed=src");
        println!("cargo:rerun-if-changed=build.rs");
        watch_git_identity();
    }
}
