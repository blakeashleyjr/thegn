//! Link clang's compiler runtime for macOS `native` builds.
//!
//! whisper-rs-sys compiles ggml's Metal backend, whose Objective-C uses `@available` checks.
//! Those lower to calls to `___isPlatformVersionAtLeast`, which lives in clang's
//! `libclang_rt.osx.a`. rustc links Apple targets with `-nodefaultlibs`, so that runtime is not
//! linked automatically, and the alpha.139 release build failed with "Undefined symbols for
//! architecture arm64: ___isPlatformVersionAtLeast". Nix's cc-wrapper adds the runtime itself,
//! which is why the Mac host's dev builds linked fine.

#![expect(
    clippy::disallowed_macros,
    reason = "a build script talks to cargo through println! directives"
)]

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" || std::env::var_os("CARGO_FEATURE_NATIVE").is_none() {
        return;
    }
    let cc = std::env::var("CC").unwrap_or_else(|_| "clang".to_owned());
    let Ok(output) = std::process::Command::new(&cc)
        .arg("--print-resource-dir")
        .output()
    else {
        println!(
            "cargo:warning=thegn-voice: could not run {cc} --print-resource-dir; not linking clang_rt.osx"
        );
        return;
    };
    if !output.status.success() {
        return;
    }
    let resource_dir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let darwin = PathBuf::from(resource_dir).join("lib").join("darwin");
    if darwin.join("libclang_rt.osx.a").is_file() {
        println!("cargo:rustc-link-search=native={}", darwin.display());
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
    } else {
        println!(
            "cargo:warning=thegn-voice: {} has no libclang_rt.osx.a; relying on the toolchain to link it",
            darwin.display()
        );
    }
}
