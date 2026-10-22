//! Architecture ratchets for the host crate (see
//! `thegn_core::test_support::ratchet`): platform-conditional code, and the
//! two render chokepoints. Each allowlist under `test/` is shrink-only and
//! explains its rule in its header.

use thegn_core::test_support::ratchet::{
    file_ratchet, has_platform_cfg, sets_state_env_directly, state_env_test_body,
};

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

/// `#[cfg(unix|windows|target_os|…)]` belongs in `src/platform/` — the one
/// seam whose job is per-OS code — so call sites stay platform-free and a new
/// OS is a new file there, not a sweep of the tree.
#[test]
fn platform_cfgs_live_in_platform_modules() {
    file_ratchet(
        MANIFEST,
        "platform-cfg-host-ratchet.txt",
        &["platform/"],
        |_, body| has_platform_cfg(body),
        "Platform-conditional code belongs in src/platform/ (CLAUDE.md: keep the \
         seam thin, call sites platform-free). Move the `#[cfg]` arm behind a \
         `platform::` function.",
    );
}

/// Colors are composed in truecolor and quantized once, at `wire.rs`'s
/// `color_spec`; the theme resolves roles to RGB. A literal anywhere else
/// bypasses both the theme and the degradation ladder.
#[test]
fn color_literals_stay_in_the_chokepoints() {
    let re = regex::Regex::new(r"Color::Rgb|Color::TrueColor|\bRgb\s*\(|\brgb\(").unwrap();
    file_ratchet(
        MANIFEST,
        "color-literal-ratchet.txt",
        // wire.rs quantizes; caps.rs holds the resolved depth; theme* resolve
        // roles; the two ratatui bridges (apps/bridge.rs blits cells,
        // apps/mod.rs converts the Palette to tg_kit::Theme) convert whole
        // palettes, which is exactly a chokepoint's job.
        &[
            "wire.rs",
            "caps.rs",
            "theme",
            "apps/bridge.rs",
            "apps/mod.rs",
        ],
        |_, body| re.is_match(body),
        "Color literals belong to the theme (a `Hue` role resolved by the active \
         palette) and are quantized once in wire.rs. Name a role instead of an RGB.",
    );
}

/// The sprite/nix ssh call sites in thegn-host (`agent.rs`, `agent_ssh.rs`,
/// `autoscale.rs`) get their host-key options from the one chokepoint; no
/// literal here. See `thegn_core::hostkey`.
#[test]
fn host_key_literals_stay_in_the_chokepoint() {
    file_ratchet(
        MANIFEST,
        "hostkey-host-ratchet.txt",
        &[],
        |_, body| thegn_core::hostkey::is_host_key_literal(body),
        "SSH host-key options belong to the one policy chokepoint \
         (`thegn_core::hostkey::host_key_args`): name a connection class and let \
         it build the `-o` args. Do not write a StrictHostKeyChecking / \
         UserKnownHostsFile / HostKeyAlias literal at a call site.",
    );
}

/// A long-lived thread that declares no QoS class runs `Interactive` — on Apple
/// silicon that makes housekeeping (samplers, watchers, reapers) eligible for
/// the performance cores it is competing with the render loop for. Every named
/// `thread::Builder` thread should declare its class on entry
/// (`platform::qos::set_self`); see the module doc in `platform/qos.rs`.
#[test]
fn long_lived_threads_declare_a_qos_class() {
    file_ratchet(
        MANIFEST,
        "thread-qos-ratchet.txt",
        // qos.rs itself spawns threads in its own tests.
        &["platform/"],
        |_, body| undeclared_named_thread(body),
        "A named long-lived thread must declare its scheduler class with \
         `crate::platform::qos::set_self(...)` as the first statement in its \
         body (CLAUDE.md; see platform/qos.rs). Add the declaration, or — with \
         a written reason on its line — pin the file.",
    );
}

fn undeclared_named_thread(body: &str) -> bool {
    (body.contains("thread::Builder::new()") || body.contains("spawn_named("))
        && !body.contains("qos::set_self")
}

#[test]
fn undeclared_named_thread_covers_builders_and_spawn_named() {
    let cases = [
        ("thread::Builder::new()", true),
        ("spawn_named(\"thegn-x\", f)", true),
        (
            "thread::Builder::new(); qos::set_self(Qos::Background)",
            false,
        ),
        (
            "spawn_named(\"thegn-x\", f); qos::set_self(Qos::Background)",
            false,
        ),
    ];

    for (body, expected) in cases {
        assert_eq!(undeclared_named_thread(body), expected, "{body}");
    }
}

#[test]
fn threads_are_named_ratchet() {
    file_ratchet(
        MANIFEST,
        "thread-name-host-ratchet.txt",
        // proc_workload_alloc.rs is compiled only from main.rs's cfg(test) module (line 209).
        &["proc_workload_alloc.rs"],
        |key, body| {
            !thegn_core::test_support::thread_spawn::is_test_module_file(key)
                && thegn_core::test_support::thread_spawn::spawns_unnamed_thread(body)
        },
        "Name production threads with `thegn_core::thread_name::spawn_named`. \
         Each allowlist entry needs a QoS decision alongside its name.",
    );
}

/// Box-drawing / block glyphs come from `caps::active_glyphs()` (Unicode or
/// ASCII, per the detected terminal). A literal bypasses the ASCII fallback.
#[test]
fn glyph_literals_go_through_active_glyphs() {
    file_ratchet(
        MANIFEST,
        "glyph-literal-ratchet.txt",
        &["caps.rs"],
        |_, body| body.chars().any(|c| ('\u{2500}'..='\u{259f}').contains(&c)),
        "Box-drawing and block glyphs must come from `caps::active_glyphs()` so \
         the ASCII fallback (`[theme] glyphs = ascii`, non-UTF-8 locales) still \
         renders. Use the GlyphSet field instead of the literal.",
    );
}

#[test]
fn state_env_ratchet_tracks_direct_test_environment_writes() {
    file_ratchet(
        MANIFEST,
        "state-env-host-ratchet.txt",
        &["testenv.rs", "test_support/ratchet.rs"],
        |key, body| state_env_test_body(key, body).is_some_and(sets_state_env_directly),
        "Tests that mutate HOME/XDG or Windows user-state variables should use \
         testenv::IsolatedState and state_env_for so isolation works on every OS.",
    );
}

fn spawn_named_without_entry_qos(body: &str) -> Vec<usize> {
    let code = thegn_core::test_support::ratchet::code_only(body);
    let lines: Vec<&str> = code.lines().collect();
    let production_end = lines
        .windows(2)
        .position(|pair| {
            pair[0].trim() == "#[cfg(test)]" && pair[1].trim_start().starts_with("mod ")
        })
        .unwrap_or(lines.len());
    lines[..production_end]
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            if !line.contains("spawn_named(") {
                return None;
            }
            let next = lines[index + 1..production_end]
                .iter()
                .find(|candidate| !candidate.trim().is_empty());
            (!next.is_some_and(|candidate| candidate.contains("qos::set_self")))
                .then_some(index + 1)
        })
        .collect()
}

#[test]
fn qos_debt_threads_declare_on_entry() {
    const FILES: &[&str] = &[
        "cmd/attach.rs",
        "env_ui.rs",
        "env_wizard.rs",
        "handlers/provision.rs",
        "hibernator.rs",
        "hook_run.rs",
        "lifecycle.rs",
        "placement_flow.rs",
        "secret.rs",
        "structural_diff.rs",
    ];

    for file in FILES {
        let path = format!("{MANIFEST}/src/{file}");
        let body = std::fs::read_to_string(path).expect("read production source file");
        assert!(body.contains("spawn_named("), "{file} has no named thread");
        assert!(
            spawn_named_without_entry_qos(&body).is_empty(),
            "{file} has a named thread without entry QoS"
        );
    }
}

#[test]
fn spawn_named_entry_qos_predicate_tracks_the_next_statement() {
    let cases = [
        (
            "spawn_named(\"thegn-x\", f);\nqos::set_self(Qos::Background);",
            vec![],
        ),
        (
            "spawn_named(\"thegn-x\", f);\nlet x = 1;\nqos::set_self(Qos::Background);",
            vec![1],
        ),
        ("spawn_named(\"thegn-x\", f);", vec![1]),
        (
            "#[cfg(test)]\nmod tests {\nspawn_named(\"thegn-x\", f);\n}",
            vec![],
        ),
    ];

    for (source, expected) in cases {
        assert_eq!(spawn_named_without_entry_qos(source), expected, "{source}");
    }
}

fn has_runtime_platform_guard(code: &str) -> bool {
    let mut remaining = code;
    while let Some(start) = remaining.find("Path::new(\"") {
        remaining = &remaining[start + "Path::new(\"".len()..];
        let Some(end_quote) = remaining.find('\"') else {
            return false;
        };
        let path = &remaining[..end_quote];
        if path != "/bin/sh" && !path.starts_with("/proc") {
            continue;
        }
        let after_path = remaining[end_quote + 1..].trim_start();
        let Some(after_close) = after_path.strip_prefix(')') else {
            continue;
        };
        let after_close = after_close.trim_start();
        let Some(after_dot) = after_close.strip_prefix('.') else {
            continue;
        };
        let after_dot = after_dot.trim_start();
        if ["exists", "is_file", "is_dir"].iter().any(|method| {
            after_dot
                .strip_prefix(method)
                .is_some_and(|tail| tail.trim_start().starts_with('('))
        }) {
            return true;
        }
    }
    false
}

#[test]
fn converted_runtime_guards_stay_compile_time() {
    const FILES: &[&str] = &[
        "pane_pty.rs",
        "pane.rs",
        "actions.rs",
        "build_cache.rs",
        "mise_provider.rs",
        "bounded_git_probe_tests.rs",
    ];

    for file in FILES {
        let path = format!("{MANIFEST}/src/{file}");
        let body = std::fs::read_to_string(path).expect("read production source file");
        let code = thegn_core::test_support::ratchet::code_only(&body);
        assert!(
            !has_runtime_platform_guard(&code),
            "{file} still probes /proc or /bin/sh at runtime"
        );
    }
}

#[test]
fn runtime_platform_guard_predicate_tracks_only_direct_probes() {
    let cases = [
        (r#"std::path::Path::new("/proc/self/stat").exists()"#, true),
        (r#"Path::new("/bin/sh").is_file()"#, true),
        (r#"Path::new("/proc").join(pid)"#, false),
        (r#"cfg!(target_os = "linux")"#, false),
        (r#"// Path::new("/proc/self/stat").exists()"#, false),
    ];

    for (source, expected) in cases {
        let code = thegn_core::test_support::ratchet::code_only(source);
        assert_eq!(has_runtime_platform_guard(&code), expected, "{source}");
    }
}

#[test]
fn thread_names_fit_the_comm_limit_ratchet() {
    file_ratchet(
        MANIFEST,
        "thread-name-length-host-ratchet.txt",
        &["proc_workload_alloc.rs"],
        |key, body| {
            !thegn_core::test_support::thread_spawn::is_test_module_file(key)
                && !thegn_core::test_support::thread_spawn::bad_thread_name_literals(body)
                    .is_empty()
        },
        "A thread name literal must start with `thegn-` and be at most 15 bytes (the Linux comm limit; longer names are truncated in /proc and break attribution). Use `thegn_core::thread_name::spawn_named`/`try_spawn_named`; the allowlist is seeded debt (THE-931) and shrink-only.",
    );
}
