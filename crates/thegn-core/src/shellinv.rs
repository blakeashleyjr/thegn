//! Shell-invocation argv building — the one place that knows how to hand a
//! command string to the *local* user shell across platforms.
//!
//! [`util::shell()`](crate::util::shell) resolves *which* shell to use
//! (`$SHELL` on unix; pwsh → powershell → `%COMSPEC%` on Windows); this module
//! resolves *how to invoke it*: POSIX shells take `-c`/`-lc`, PowerShell takes
//! `-NoProfile -Command`, and cmd.exe takes `/C`. Callers building an argv for
//! a **local** pane/pin/spawn must go through here; call sites that target a
//! remote or sandboxed *Linux* environment keep their literal `sh -lc` (the
//! target substrate is known, not the host's).
//!
//! Pure and I/O-free so it compiles and unit-tests identically on every
//! platform (the Windows arms are exercised by Linux CI).

/// How a shell expects an inline command: the flag dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFlavor {
    /// `sh`/`bash`/`zsh`/`fish`/… — `-c` (and `-l` for a login shell).
    Posix,
    /// PowerShell (`pwsh.exe` / `powershell.exe`) — `-NoProfile -Command`.
    Pwsh,
    /// `cmd.exe` — `/C`.
    Cmd,
}

/// Classify a shell program (path or bare name) into its invocation dialect.
/// Matching is on the file stem, case-insensitively, so `C:\...\PWSH.EXE`,
/// `pwsh`, and `/usr/bin/pwsh` all classify the same. The basename split is
/// manual (both `/` and `\`) rather than `std::path` — unix `Path` treats `\`
/// as an ordinary character, and this function must classify Windows paths
/// identically on every platform (the Windows arms are unit-tested on Linux).
pub fn flavor_of(shell: &str) -> ShellFlavor {
    let base = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    let stem = base
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .to_string();
    match stem.as_str() {
        "pwsh" | "powershell" => ShellFlavor::Pwsh,
        "cmd" => ShellFlavor::Cmd,
        _ => ShellFlavor::Posix,
    }
}

/// Argv that runs `cmd` through `shell` non-interactively.
pub fn run_argv(shell: &str, cmd: &str) -> Vec<String> {
    match flavor_of(shell) {
        ShellFlavor::Posix => vec![shell.into(), "-c".into(), cmd.into()],
        ShellFlavor::Pwsh => vec![
            shell.into(),
            "-NoProfile".into(),
            "-Command".into(),
            cmd.into(),
        ],
        ShellFlavor::Cmd => vec![shell.into(), "/C".into(), cmd.into()],
    }
}

/// Split a command into an argv only when doing so cannot invoke shell syntax.
///
/// Gate commands are normally intentionally shell commands, so this is a
/// conservative fast path rather than a shell parser. It accepts ASCII
/// whitespace-separated words, rejects shell operators, quoting, expansion,
/// globbing, and a leading environment-assignment word, and leaves all other
/// word contents untouched.
pub fn direct_argv(command: &str) -> Option<Vec<String>> {
    if command.contains(['\n', '\r']) {
        return None;
    }
    let argv: Vec<String> = command
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect();
    let first = argv.first()?;
    if first.contains('=') {
        return None;
    }
    const SHELL_WORDS: &[&str] = &[
        "exit", "cd", ".", "source", "export", "unset", "set", "eval", "exec", "command",
        "builtin", "alias", "unalias", "trap", "return", "shift", "read", "wait", "ulimit",
        "umask", "type", "hash", "local", "readonly", "declare", "typeset", "times", "break",
        "continue", "if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "until",
        "do", "done", "function", "select", "time", "coproc", "in", "[[", "]]",
    ];
    if SHELL_WORDS.contains(&first.as_str()) {
        return None;
    }
    if argv.iter().any(|word| {
        word.chars().any(|character| {
            matches!(
                character,
                '|' | '&'
                    | ';'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '$'
                    | '`'
                    | '\\'
                    | '\''
                    | '"'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '!'
                    | '~'
                    | '#'
            )
        })
    }) {
        return None;
    }
    Some(argv)
}

/// Argv that runs `cmd` through a *login* `shell`, `exec`-replacing the shell
/// with the program where the platform supports it (so signals reach the
/// program, not the wrapper). On PowerShell/cmd there is no login mode and no
/// `exec`; the plain [`run_argv`] shape is the closest equivalent.
pub fn exec_argv(shell: &str, cmd: &str) -> Vec<String> {
    match flavor_of(shell) {
        ShellFlavor::Posix => vec![shell.into(), "-lc".into(), format!("exec {cmd}")],
        ShellFlavor::Pwsh | ShellFlavor::Cmd => run_argv(shell, cmd),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_flavors_by_stem_case_insensitively() {
        for (shell, want) in [
            ("/bin/sh", ShellFlavor::Posix),
            ("/usr/bin/zsh", ShellFlavor::Posix),
            ("bash", ShellFlavor::Posix),
            ("fish", ShellFlavor::Posix),
            ("pwsh", ShellFlavor::Pwsh),
            ("pwsh.exe", ShellFlavor::Pwsh),
            (r"C:\Program Files\PowerShell\7\PWSH.EXE", ShellFlavor::Pwsh),
            ("powershell.exe", ShellFlavor::Pwsh),
            (r"C:\Windows\System32\cmd.exe", ShellFlavor::Cmd),
            ("cmd", ShellFlavor::Cmd),
        ] {
            assert_eq!(flavor_of(shell), want, "shell {shell:?}");
        }
    }

    #[test]
    fn run_argv_matches_each_dialect() {
        assert_eq!(run_argv("/bin/sh", "echo hi"), ["/bin/sh", "-c", "echo hi"]);
        assert_eq!(
            run_argv("pwsh.exe", "echo hi"),
            ["pwsh.exe", "-NoProfile", "-Command", "echo hi"]
        );
        assert_eq!(run_argv("cmd.exe", "echo hi"), ["cmd.exe", "/C", "echo hi"]);
    }

    #[test]
    fn exec_argv_login_execs_on_posix_only() {
        assert_eq!(
            exec_argv("/bin/zsh", "yazi"),
            ["/bin/zsh", "-lc", "exec yazi"]
        );
        // No login mode / exec on Windows shells: same shape as run_argv.
        assert_eq!(
            exec_argv("pwsh.exe", "yazi"),
            ["pwsh.exe", "-NoProfile", "-Command", "yazi"]
        );
        assert_eq!(exec_argv("cmd.exe", "yazi"), ["cmd.exe", "/C", "yazi"]);
    }

    #[test]
    fn direct_argv_accepts_plain_words_and_rejects_shell_syntax() {
        for (command, want) in [
            ("cargo test", Some(vec!["cargo".into(), "test".into()])),
            (
                "  ./gate --format=json\t--quiet  ",
                Some(vec![
                    "./gate".into(),
                    "--format=json".into(),
                    "--quiet".into(),
                ]),
            ),
            ("echo foo=bar", Some(vec!["echo".into(), "foo=bar".into()])),
            ("", None),
            ("   \t", None),
            ("FOO=bar cargo test", None),
            ("exit 1", None),
            ("cd foo", None),
            ("source env.sh", None),
            ("echo 'hello'", None),
            ("echo \"hello\"", None),
            ("echo hi && exit 1", None),
            ("echo *.rs", None),
            ("echo $HOME", None),
            ("echo hi\nexit 1", None),
        ] {
            assert_eq!(direct_argv(command), want, "command {command:?}");
        }
    }
}
