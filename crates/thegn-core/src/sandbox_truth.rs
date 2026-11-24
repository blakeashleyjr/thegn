//! **What a pane is actually running in**, derived from the argv about to be
//! executed rather than from what the user asked for.
//!
//! The bug this exists to make impossible: a terminal created with an explicit
//! `podman-rootless` pick, on a Mac with no podman machine running, resolved
//! through the backend chain to [`Backend::None`], spawned a bare `sh -lc 'cd
//! … && exec $SHELL'` — and was still labelled `podman-rootless`, because the
//! label was copied from the REQUEST. A containment label that can disagree
//! with reality is worse than no label: it tells someone their agent is
//! sandboxed while it runs on the host with no kernel boundary.
//!
//! So containment is reported from one source only — the argv. [`observed`]
//! reads the runtime out of the command words of a launch argv, and
//! [`reconcile`] compares it against the request to produce the label, the
//! degraded flag, and the warning a caller must surface. Neither function can
//! return a container label for an argv that does not run a container:
//! `sandbox_truth_tests::every_backend_round_trips` walks every [`Backend`],
//! renders its real [`crate::sandbox::enter_argv`], and asserts the observed
//! backend matches — so a new backend, or a change to how one is spelled, fails
//! the build instead of quietly re-opening the lie.
//!
//! **Limit, stated rather than papered over:** the Windows Job Object contains
//! a process through the spawn syscall, not through the argv, so no argv
//! inspection can see it. [`reconcile`] takes it at its word and says so here.

use crate::sandbox::Backend;

/// Words that keep the *next* word in command position (`sudo -n podman …`,
/// `exec zsh`), rather than being the command themselves.
const PASSTHROUGH: &[&str] = &[
    "sudo", "doas", "env", "exec", "nice", "nohup", "command", "time", "stdbuf",
];

fn basename(s: &str) -> &str {
    let s = s.trim_matches(|c| c == '\'' || c == '"' || c == '(');
    s.rsplit(['/', '\\']).next().unwrap_or(s)
}

/// Is `tok` a Windows-style command token: a `\` in the path, or an `.exe`
/// suffix (case-insensitive)? POSIX tokens never match, so Linux labels are
/// unchanged.
fn is_windows_style(tok: &str) -> bool {
    let t = tok.trim_matches(|c| c == '\'' || c == '"' || c == '(');
    t.contains('\\') || t.to_ascii_lowercase().ends_with(".exe")
}

/// The command word for `tok`: its basename, and for a Windows-style token the
/// lowercased name with `.exe` stripped (`C:\...\DOCKER.EXE` -> `docker`).
///
/// `wsl`/`wslc` keep an `.exe` spelling so only the Windows binary maps to
/// [`Backend::Wsl`] (a bare POSIX `wsl` is unchanged). `container` also keeps
/// `.exe`, so it can never match Apple's `container` word.
// [kb] `wslc` `container.exe` alias is unsourced; defensive: never map
// `container.exe` to Apple on Windows.
fn command_word(tok: &str) -> String {
    let w = basename(tok);
    if !is_windows_style(tok) {
        return w.to_string();
    }
    let lower = w.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    match stem {
        "wsl" | "wslc" | "container" => format!("{stem}.exe"),
        _ => stem.to_string(),
    }
}

/// Is `w` a token that decorates a command rather than naming one — a flag
/// (`-it`) or a leading env assignment (`FOO=bar`)?
fn is_decoration(w: &str) -> bool {
    w.starts_with('-') || (w.contains('=') && !w.starts_with('/'))
}

/// The command words of a shell script body: the first real word of each
/// command in the pipeline/list, skipping pass-throughs.
///
/// Only command position is considered, which is what keeps a worktree path
/// from being mistaken for a runtime: `cd /Users/me/code/docker && exec zsh`
/// yields `cd`, `exec`, `zsh` — the path is an argument to `cd`, never a
/// command, so it can never be read as "this pane is in Docker".
fn script_command_words(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in script
        .split([';', '\n'])
        .flat_map(|p| p.split("&&"))
        .flat_map(|p| p.split("||"))
        .flat_map(|p| p.split('|'))
    {
        for tok in part.split_whitespace() {
            if is_decoration(tok) {
                continue;
            }
            let w = command_word(tok);
            if w.is_empty() {
                continue;
            }
            let passthrough = PASSTHROUGH.contains(&w.as_str());
            out.push(w);
            if !passthrough {
                // The command for this part is named; the rest are its arguments.
                break;
            }
        }
    }
    out
}

/// Every word of `argv` that sits in command position — `argv[0]`, whatever a
/// pass-through hands off to, and the command words of any embedded script.
fn command_words(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut expect_command = true;
    for el in argv {
        // A Windows path with spaces (`C:\Program Files\Docker\docker.exe`) is
        // one command word, not a script body.
        let win_path = expect_command
            && el.contains('\\')
            && el
                .trim_matches(|c| c == '\'' || c == '"')
                .to_ascii_lowercase()
                .ends_with(".exe");
        if el.contains(char::is_whitespace) && !win_path {
            // A script body (`sh -lc <script>`, or the remote command an ssh
            // placement appends): scan inside it.
            out.extend(script_command_words(el));
            expect_command = false;
            continue;
        }
        if el == "--" {
            // End-of-flags separator: what follows is a command again. This is
            // how a transport hands off to the runtime (`kubectl exec … -- podman
            // exec …`, `wsl.exe -- podman …`), so without it a remote container
            // pane reads as an uncontained host shell.
            expect_command = true;
            continue;
        }
        if !expect_command {
            continue;
        }
        if is_decoration(el) {
            // A pass-through's own flags (`sudo -n`) keep the expectation open.
            continue;
        }
        let w = command_word(el);
        expect_command = PASSTHROUGH.contains(&w.as_str());
        out.push(w);
    }
    out
}

/// The containment [`Backend`] this argv actually enters, or [`Backend::None`]
/// when it runs straight on the host.
///
/// Reads only command words (see the private `command_words`), so an argument
/// that merely
/// contains a runtime's name — a path, a git remote, a script's text — cannot
/// promote a host shell into a claimed container.
pub fn observed(argv: &[String]) -> Backend {
    // A resource ceiling is not containment. `sandbox_cpucap` prepends either
    // `systemd-run --scope …` or the soft `nice -n …` prefix to host-toolchain
    // backends, which sits in front of the real runtime word. See `strip_cap`
    // for why removing either exact prefix can never erase a real sandbox.
    let argv = crate::sandbox_cpucap::strip_cap(argv);
    let mut sudo = false;
    for w in command_words(argv) {
        match w.as_str() {
            "sudo" | "doas" => sudo = true,
            // Rootful podman is spelled `sudo -n podman …`; rootless is the
            // bare binary. The distinction is the whole point of the two
            // backends, so it must survive the round trip.
            "podman" => {
                return if sudo {
                    Backend::PodmanRootful
                } else {
                    Backend::Podman
                };
            }
            "docker" => return Backend::Docker,
            "bwrap" => return Backend::Bwrap,
            "systemd-run" => return Backend::Systemd,
            // Apple's runtime CLI is literally `container`.
            "container" => return Backend::Apple,
            // The reserved smol: the real smolvm binary and the legacy word.
            "smolvm" | "smolmachines" => return Backend::Smol,
            "wsl.exe" | "wslc.exe" => return Backend::Wsl,
            _ => {}
        }
    }
    Backend::None
}

/// The verdict on a launch: the label to record and display, whether the
/// request was honoured, and the note a user must be shown when it was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truth {
    /// The containment label — always what the argv actually does.
    pub label: String,
    /// The request was NOT honoured (asked for containment, got something else).
    pub degraded: bool,
    /// Human-facing explanation, present exactly when `degraded`.
    pub warning: Option<String>,
}

/// Whether `requested` (a config/DB backend name) asks for real containment.
/// `""`, `auto`, `host` and `none` do not: they are "wherever the chain lands",
/// so landing on the host is not a degradation.
fn requests_containment(requested: &str) -> bool {
    !matches!(requested.trim(), "" | "auto" | "host" | "none")
}

/// Compare what was asked for against what the argv actually enters.
///
/// The returned [`Truth::label`] is derived from `argv` in every case where the
/// argv can show containment — callers must record and display THAT, never the
/// request. See the module docs for the one exception (the Windows Job Object,
/// whose scoping is invisible to argv inspection).
///
/// **Call this for LOCAL placements.** A remote placement (ssh/k8s/provider)
/// runs its container on another machine behind a transport whose argv shape is
/// the remote's business; observation there can produce a false `host` — the
/// safe direction, but a wrong label all the same. Remote launches should keep
/// the resolver's label, which the placement's own bring-up already proved.
pub fn reconcile(requested: &str, argv: &[String]) -> Truth {
    // Lenient: a reserved request (smol, wsl, appcontainer) is still named so
    // the warning can say why it was not honoured.
    let want = crate::config::SandboxBackend::from_str_any(requested.trim())
        .and_then(Backend::from_config);
    // Job Object containment happens in the spawn syscall, so argv inspection
    // cannot see it. AppContainer is reserved and must be reported as host.
    if want == Some(Backend::WinJobObject) {
        return Truth {
            label: want.map(|b| b.label().to_string()).unwrap_or_default(),
            degraded: false,
            warning: None,
        };
    }
    let got = observed(argv);
    let label = got.label().to_string();
    if !requests_containment(requested) || want == Some(got) {
        return Truth {
            label,
            degraded: false,
            warning: None,
        };
    }
    let mut warning = if got == Backend::None {
        format!(
            "sandbox '{}' unavailable — this pane is running on the host (no kernel boundary)",
            requested.trim()
        )
    } else {
        format!(
            "sandbox '{}' unavailable — this pane fell back to '{}'",
            requested.trim(),
            label
        )
    };
    if let Some(reserved) = want.filter(|backend| backend.is_reserved()) {
        warning.push_str(&format!(
            " ({} is reserved and not implemented)",
            reserved.label()
        ));
    }
    Truth {
        label,
        degraded: true,
        warning: Some(warning),
    }
}

#[cfg(test)]
#[path = "sandbox_truth_tests.rs"]
mod sandbox_truth_tests;
