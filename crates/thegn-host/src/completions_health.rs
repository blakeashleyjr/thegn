//! Is the installed shell-completion file current? — the detection half of the
//! completions story.
//!
//! Delivery moved to the packager (a package regenerates the completion file
//! with the binary, so a packaged install cannot drift), but a `cargo install`,
//! a raw tarball or a hand-copied script genuinely can. The repo's answer to
//! "is this subsystem actually working?" is always `thegn doctor`, so that is
//! where staleness is reported — detection beats asking every user to re-source
//! a script at every shell launch forever.
//!
//! Two pure functions ([`search_paths`], [`classify`]) plus one thin I/O
//! wrapper ([`report`]), so the logic is testable without a filesystem.
//!
//! **Never compare mtimes.** Nix normalises store timestamps to 1970-01-01, so
//! "installed file older than the binary" reports every correct Nix install as
//! stale. Content comparison is the only test that works across install paths.

use std::path::{Path, PathBuf};

/// The env-var assignment a dynamic registration shim carries in its body
/// (`COMPLETE="zsh" thegn …`). Its presence means the script asks the binary at
/// completion time and therefore *cannot* go stale, whatever it contains.
///
/// Deliberately a local constant rather than a reach into [`crate::complete`]:
/// this check must stay correct for a file written by any thegn version, which
/// is a property of the emitted script, not of today's emitter.
const SHIM_MARKER: &str = "COMPLETE=";

/// The command names one binary answers to. A script registered for `thegn`
/// never fires for `tg`, so both are reported separately.
pub const COMMANDS: [&str; 2] = ["thegn", "tg"];

/// The same pair on the dev channel, which renames both so a dev build can sit
/// beside a stable one (`nix/package.nix`).
const DEV_COMMANDS: [&str; 2] = ["thegn-dev", "tg-dev"];

/// The command names to report for the binary at `exe`.
///
/// Not a constant, because the dev channel is installed as `thegn-dev` / `tg-dev`
/// and its completion files are named for *those* commands. Reporting a dev
/// install against the stable names would call a perfectly good install
/// `absent` and print a fix command naming a binary the user does not have.
pub fn commands_for(exe: &Path) -> [&'static str; 2] {
    match exe.file_name().and_then(|n| n.to_str()) {
        // `starts_with`, for the `.exe` suffix on Windows.
        Some(name) if name.starts_with("thegn-dev") || name.starts_with("tg-dev") => DEV_COMMANDS,
        _ => COMMANDS,
    }
}

/// The shells with a conventional, discoverable install location. Elvish and
/// PowerShell have none (see `docs/cli.md`), so there is nothing to look for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    pub const ALL: [Shell; 3] = [Shell::Zsh, Shell::Bash, Shell::Fish];

    pub fn as_str(self) -> &'static str {
        match self {
            Shell::Zsh => "zsh",
            Shell::Bash => "bash",
            Shell::Fish => "fish",
        }
    }

    /// The generator this shell maps to, for producing the comparison script.
    fn clap(self) -> clap_complete::Shell {
        match self {
            Shell::Zsh => clap_complete::Shell::Zsh,
            Shell::Bash => clap_complete::Shell::Bash,
            Shell::Fish => clap_complete::Shell::Fish,
        }
    }

    /// The file name the shell looks for, for a command called `cmd`.
    fn file_name(self, cmd: &str) -> String {
        match self {
            Shell::Zsh => format!("_{cmd}"),
            Shell::Bash => cmd.to_string(),
            Shell::Fish => format!("{cmd}.fish"),
        }
    }
}

/// One place a completion file for `(shell, command)` could live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub shell: Shell,
    pub command: &'static str,
    pub dir: PathBuf,
    pub file_name: String,
}

impl Target {
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }

    /// The command that (re)writes this file. Generated *as the command name*,
    /// so the `tg` script defines a `tg` shell function.
    pub fn fix_command(&self) -> String {
        format!(
            "{} completions {} > {}",
            self.command,
            self.shell.as_str(),
            self.path().display()
        )
    }
}

/// What an installed file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Byte-equal to what this binary generates.
    Fresh,
    /// Present and different — it predates verbs this binary has.
    Stale,
    /// A registration shim: it asks the binary at completion time, so it can
    /// never go stale and is not diffed.
    Dynamic,
    /// Nothing found in any searched location.
    Absent,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Fresh => "fresh",
            State::Stale => "stale",
            State::Dynamic => "dynamic",
            State::Absent => "absent",
        }
    }

    /// Fresh and dynamic both mean "nothing to do".
    pub fn is_healthy(self) -> bool {
        matches!(self, State::Fresh | State::Dynamic)
    }
}

/// The environment [`search_paths`] reads, injected so tests never mutate the
/// process env (the `thegn_core::config::ProcessEnv` pattern).
#[derive(Debug, Default, Clone)]
pub struct Env {
    pub home: Option<String>,
    pub xdg_data_home: Option<String>,
    pub xdg_config_home: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        Self {
            home: get("HOME"),
            xdg_data_home: get("XDG_DATA_HOME"),
            xdg_config_home: get("XDG_CONFIG_HOME"),
        }
    }

    /// `$XDG_DATA_HOME`, else `~/.local/share`.
    fn data_home(&self) -> Option<PathBuf> {
        self.xdg_data_home.as_ref().map(PathBuf::from).or_else(|| {
            self.home
                .as_ref()
                .map(|h| Path::new(h).join(".local/share"))
        })
    }

    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    fn config_home(&self) -> Option<PathBuf> {
        self.xdg_config_home
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| self.home.as_ref().map(|h| Path::new(h).join(".config")))
    }

    fn home_join(&self, rel: &str) -> Option<PathBuf> {
        self.home.as_ref().map(|h| Path::new(h).join(rel))
    }
}

/// The install prefix a binary at `exe` belongs to: walk up out of `bin/`.
///
/// This is what finds a Nix-store or `~/.local` install — the case that matters
/// most, since that is where a packager put the completions. The caller
/// resolves symlinks first (`current_exe()` on a `tg` invocation lands on the
/// real binary, which is the one whose prefix holds the data files).
pub fn prefix_of(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    bin.parent().map(Path::to_path_buf)
}

/// Every place a completion file for each `(shell, command)` could live, in
/// priority order: user locations first, then this install's prefix, then the
/// system ones.
pub fn search_paths(env: &Env, exe: &Path) -> Vec<Target> {
    let prefix = prefix_of(exe);
    let mut out = Vec::new();
    for shell in Shell::ALL {
        let dirs: Vec<PathBuf> = match shell {
            Shell::Zsh => [
                env.data_home().map(|d| d.join("zsh/site-functions")),
                env.home_join(".zsh/completions"),
                // The location `docs/cli.md` tells a hand-installer to use.
                env.home_join(".zfunc"),
                prefix.as_ref().map(|p| p.join("share/zsh/site-functions")),
                Some(PathBuf::from("/usr/local/share/zsh/site-functions")),
                Some(PathBuf::from("/usr/share/zsh/site-functions")),
            ],
            Shell::Bash => [
                env.data_home()
                    .map(|d| d.join("bash-completion/completions")),
                env.home_join(".local/share/bash-completion/completions"),
                prefix
                    .as_ref()
                    .map(|p| p.join("share/bash-completion/completions")),
                Some(PathBuf::from("/etc/bash_completion.d")),
                Some(PathBuf::from("/usr/share/bash-completion/completions")),
                None,
            ],
            Shell::Fish => [
                env.config_home().map(|c| c.join("fish/completions")),
                env.home_join(".config/fish/completions"),
                prefix
                    .as_ref()
                    .map(|p| p.join("share/fish/vendor_completions.d")),
                Some(PathBuf::from("/usr/share/fish/vendor_completions.d")),
                None,
                None,
            ],
        }
        .into_iter()
        .flatten()
        // `$XDG_CONFIG_HOME` unset makes the first two fish entries the same
        // path; a duplicate would be a duplicate report row.
        .fold(Vec::new(), |mut acc: Vec<PathBuf>, d| {
            if !acc.contains(&d) {
                acc.push(d);
            }
            acc
        });
        for command in commands_for(exe) {
            for dir in &dirs {
                out.push(Target {
                    shell,
                    command,
                    dir: dir.clone(),
                    file_name: shell.file_name(command),
                });
            }
        }
    }
    out
}

/// What an installed script body is, relative to what this binary generates.
///
/// The shim check comes first and never diffs: it is correct whether the
/// installed file is a self-contained `--static` script or a registration shim,
/// which is what keeps this decoupled from how completions are emitted.
///
/// Trailing whitespace is normalised away — a packager or an editor may have
/// added a newline, and reporting `stale` for a `\n` is noise.
pub fn classify(installed: &[u8], generated: &[u8]) -> State {
    if is_shim(installed) {
        return State::Dynamic;
    }
    if installed.trim_ascii_end() == generated.trim_ascii_end() {
        State::Fresh
    } else {
        State::Stale
    }
}

/// Whether an installed script is a registration shim. Split out of
/// [`classify`] so the caller can recognise one *before* paying to generate the
/// script it would otherwise be compared against — a packaged install is all
/// shims, and that is the common case.
pub fn is_shim(installed: &[u8]) -> bool {
    contains(installed, SHIM_MARKER.as_bytes())
}

/// Naive substring search — the haystack is a few KiB of shell script, once,
/// in `doctor`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// One reported `(shell, command)` pair.
#[derive(Debug, Clone)]
pub struct Row {
    pub shell: Shell,
    pub command: &'static str,
    pub state: State,
    /// Where the file was found, or — for `absent` — where to write it.
    pub path: PathBuf,
    /// The command that fixes a `stale`/`absent` row, with the path filled in.
    pub fix: String,
}

/// The whole report: one row per `(shell, command)`.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub rows: Vec<Row>,
}

impl Report {
    /// Rows that need the user to do something.
    pub fn needs_attention(&self) -> bool {
        self.rows.iter().any(|r| !r.state.is_healthy())
    }
}

/// Walk the targets and classify the first existing file per `(shell, command)`.
///
/// Every read is best-effort: an unreadable file is treated as absent, never as
/// a doctor failure. Generation is lazy — an absent install never pays for it,
/// and neither does a registration shim ([`is_shim`] settles those without a
/// comparison) — and always into a `Vec<u8>`, because the `clap_complete`
/// generators panic on a write error.
pub fn report() -> Report {
    let exe = std::env::current_exe()
        .map(|e| std::fs::canonicalize(&e).unwrap_or(e))
        .unwrap_or_default();
    report_with(&Env::from_process(), &exe, &mut generate)
}

/// [`report`] with the environment, the exe path, and the generator injected —
/// the seam the tempdir round-trip test drives.
pub fn report_with(
    env: &Env,
    exe: &Path,
    gen_script: &mut dyn FnMut(Shell, &str) -> Vec<u8>,
) -> Report {
    let targets = search_paths(env, exe);
    let mut rows = Vec::new();
    for shell in Shell::ALL {
        for command in commands_for(exe) {
            let mine = targets
                .iter()
                .filter(|t| t.shell == shell && t.command == command);
            let mut row = None;
            let mut first: Option<&Target> = None;
            for target in mine {
                if first.is_none() {
                    first = Some(target);
                }
                let Ok(installed) = std::fs::read(target.path()) else {
                    continue;
                };
                // The shim check first, so a packaged install (every row a
                // shim) never generates a script only to discard it.
                let state = if is_shim(&installed) {
                    State::Dynamic
                } else {
                    classify(&installed, &gen_script(shell, command))
                };
                row = Some(Row {
                    shell,
                    command,
                    state,
                    path: target.path(),
                    fix: target.fix_command(),
                });
                break;
            }
            rows.push(row.unwrap_or_else(|| {
                let (path, fix) = first
                    .map(|t| (t.path(), t.fix_command()))
                    .unwrap_or_default();
                Row {
                    shell,
                    command,
                    state: State::Absent,
                    path,
                    fix,
                }
            }));
        }
    }
    Report { rows }
}

/// The script this binary would emit for `(shell, command)` today, over the
/// same grouped tree the parser uses.
fn generate(shell: Shell, command: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tree = crate::cli_help::attach(<crate::Cli as clap::CommandFactory>::command());
    clap_complete::aot::generate(shell.clap(), &mut tree, command, &mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &str) -> Env {
        Env {
            home: Some(home.into()),
            ..Env::default()
        }
    }

    fn dirs_for(targets: &[Target], shell: Shell, command: &str) -> Vec<String> {
        targets
            .iter()
            .filter(|t| t.shell == shell && t.command == command)
            .map(|t| t.path().display().to_string())
            .collect()
    }

    #[test]
    fn search_paths_honour_xdg_when_set() {
        let e = Env {
            home: Some("/home/u".into()),
            xdg_data_home: Some("/xdg/data".into()),
            xdg_config_home: Some("/xdg/config".into()),
        };
        let t = search_paths(&e, Path::new("/opt/thegn/bin/thegn"));
        assert_eq!(
            dirs_for(&t, Shell::Zsh, "thegn")[0],
            "/xdg/data/zsh/site-functions/_thegn"
        );
        assert_eq!(
            dirs_for(&t, Shell::Bash, "thegn")[0],
            "/xdg/data/bash-completion/completions/thegn"
        );
        assert_eq!(
            dirs_for(&t, Shell::Fish, "thegn")[0],
            "/xdg/config/fish/completions/thegn.fish"
        );
    }

    #[test]
    fn search_paths_fall_back_to_home_when_xdg_unset() {
        let t = search_paths(&env("/home/u"), Path::new("/opt/thegn/bin/thegn"));
        assert_eq!(
            dirs_for(&t, Shell::Zsh, "thegn")[0],
            "/home/u/.local/share/zsh/site-functions/_thegn"
        );
        assert_eq!(
            dirs_for(&t, Shell::Bash, "thegn")[0],
            "/home/u/.local/share/bash-completion/completions/thegn"
        );
        // The XDG default and the literal `~/.config` entry are the same path;
        // it appears once.
        let fish = dirs_for(&t, Shell::Fish, "thegn");
        assert_eq!(fish[0], "/home/u/.config/fish/completions/thegn.fish");
        assert_eq!(
            fish.iter()
                .filter(|p| p.starts_with("/home/u/.config"))
                .count(),
            1
        );
    }

    #[test]
    fn search_paths_cover_both_command_names() {
        let t = search_paths(&env("/home/u"), Path::new("/opt/thegn/bin/thegn"));
        for shell in Shell::ALL {
            for command in COMMANDS {
                assert!(
                    !dirs_for(&t, shell, command).is_empty(),
                    "no {} targets for {command}",
                    shell.as_str()
                );
            }
        }
        assert!(
            dirs_for(&t, Shell::Zsh, "tg")
                .iter()
                .all(|p| p.ends_with("_tg"))
        );
        assert!(
            dirs_for(&t, Shell::Fish, "tg")
                .iter()
                .all(|p| p.ends_with("tg.fish"))
        );
    }

    #[test]
    fn search_paths_derive_the_install_prefix_from_the_exe() {
        let t = search_paths(&env("/home/u"), Path::new("/nix/store/abc-thegn/bin/thegn"));
        assert!(
            dirs_for(&t, Shell::Zsh, "thegn")
                .contains(&"/nix/store/abc-thegn/share/zsh/site-functions/_thegn".to_string())
        );
        assert!(
            dirs_for(&t, Shell::Fish, "tg").contains(
                &"/nix/store/abc-thegn/share/fish/vendor_completions.d/tg.fish".to_string()
            )
        );
    }

    /// A dev-channel install is reported under the names it was actually
    /// installed as — otherwise doctor calls a correct install `absent` and
    /// offers a fix command naming a binary that is not there.
    #[test]
    fn the_dev_channel_is_reported_under_its_own_command_names() {
        let dev = Path::new("/nix/store/abc-tg-dev/bin/thegn-dev");
        assert_eq!(commands_for(dev), ["thegn-dev", "tg-dev"]);
        assert_eq!(commands_for(Path::new("/opt/x/bin/thegn")), COMMANDS);
        assert_eq!(commands_for(Path::new("thegn-dev.exe")), DEV_COMMANDS);

        let t = search_paths(&env("/home/u"), dev);
        assert_eq!(
            dirs_for(&t, Shell::Zsh, "thegn-dev")[0],
            "/home/u/.local/share/zsh/site-functions/_thegn-dev"
        );
        assert!(dirs_for(&t, Shell::Zsh, "thegn").is_empty());
        // And the fix command names the binary the user actually has.
        let row = report_with(&env("/home/u"), dev, &mut |_, _| b"x".to_vec());
        assert!(
            row.rows
                .iter()
                .all(|r| r.command == "thegn-dev" || r.command == "tg-dev")
        );
        assert!(row.rows[0].fix.starts_with("thegn-dev completions "));
    }

    #[test]
    fn prefix_needs_a_bin_parent() {
        assert_eq!(
            prefix_of(Path::new("/opt/x/bin/thegn")),
            Some(PathBuf::from("/opt/x"))
        );
        // A `cargo run` binary lives in `target/debug`, not a prefix.
        assert_eq!(prefix_of(Path::new("/w/target/debug/thegn")), None);
        // A prefix-less exe still yields the user + system locations.
        let t = search_paths(&env("/home/u"), Path::new("/w/target/debug/thegn"));
        assert!(!dirs_for(&t, Shell::Zsh, "thegn").is_empty());
    }

    #[test]
    fn classify_identical_bytes_is_fresh() {
        assert_eq!(
            classify(b"#compdef thegn\n", b"#compdef thegn\n"),
            State::Fresh
        );
    }

    #[test]
    fn classify_a_missing_verb_is_stale() {
        assert_eq!(
            classify(b"'wt:worktrees'", b"'wt:worktrees' 'land:land'"),
            State::Stale
        );
    }

    #[test]
    fn classify_ignores_trailing_whitespace() {
        assert_eq!(classify(b"script\n\n", b"script"), State::Fresh);
        assert_eq!(classify(b"script", b"script  \n"), State::Fresh);
    }

    #[test]
    fn classify_a_shim_is_dynamic_even_when_it_differs() {
        let shim = br#"_clap_complete_thegn() { COMPLETE="zsh" "thegn" -- "$@"; }"#;
        assert_eq!(classify(shim, b"anything else at all"), State::Dynamic);
        // …and a `--static` script is never mistaken for one.
        assert_eq!(
            classify(b"COMPREPLY=( $(compgen -W ...) )", b"other"),
            State::Stale
        );
    }

    /// The whole wrapper against a real filesystem: a written script reports
    /// `fresh`, one mutated byte reports `stale`.
    #[test]
    fn report_round_trips_through_a_real_directory() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join(".local/share/zsh/site-functions");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let script = b"#compdef thegn\n_thegn() { :; }\n";
        std::fs::write(dir.join("_thegn"), script).expect("write");

        let e = env(&home.path().display().to_string());
        let exe = home.path().join("bin/thegn");
        let mut gen_script = |_: Shell, _: &str| script.to_vec();

        let r = report_with(&e, &exe, &mut gen_script);
        let zsh_thegn = |r: &Report| {
            r.rows
                .iter()
                .find(|row| row.shell == Shell::Zsh && row.command == "thegn")
                .expect("row")
                .clone()
        };
        let row = zsh_thegn(&r);
        assert_eq!(row.state, State::Fresh);
        assert_eq!(row.path, dir.join("_thegn"));

        // Everything else is absent, and an absent row still names a fix.
        let bash = r
            .rows
            .iter()
            .find(|row| row.shell == Shell::Bash && row.command == "tg")
            .expect("row");
        assert_eq!(bash.state, State::Absent);
        assert!(bash.fix.starts_with("tg completions bash > "));
        assert!(r.needs_attention());

        std::fs::write(dir.join("_thegn"), b"#compdef thegn\n_thegn() { drift; }\n")
            .expect("write");
        assert_eq!(
            zsh_thegn(&report_with(&e, &exe, &mut gen_script)).state,
            State::Stale
        );
    }

    /// A shim is settled by [`is_shim`] alone: `report_with` must not generate
    /// a script to compare it against. That is the packaged case — six rows,
    /// all shims — so a regression here puts six `aot` generations into every
    /// `thegn doctor` run for an answer it throws away.
    #[test]
    fn a_shim_row_costs_no_generation() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join(".local/share/zsh/site-functions");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("_thegn"),
            br#"_clap_complete_thegn() { COMPLETE="zsh" "thegn" -- "$@"; }"#,
        )
        .expect("write");

        let mut generated = 0usize;
        let mut gen_script = |_: Shell, _: &str| {
            generated += 1;
            b"anything".to_vec()
        };
        let r = report_with(
            &env(&home.path().display().to_string()),
            &home.path().join("bin/thegn"),
            &mut gen_script,
        );
        assert!(r.rows.iter().any(|row| row.shell == Shell::Zsh
            && row.command == "thegn"
            && row.state == State::Dynamic));
        // Every other row is absent, which also never generates.
        assert_eq!(generated, 0);
        assert!(!r.rows.iter().any(|row| row.state == State::Stale));
    }

    /// The real generator produces a script for every reported shell, and the
    /// self-contained one is never mistaken for a shim.
    #[test]
    fn the_real_generator_answers_for_every_shell() {
        for shell in Shell::ALL {
            for command in COMMANDS {
                let script = generate(shell, command);
                assert!(!script.is_empty(), "{} {command}", shell.as_str());
                assert_eq!(classify(&script, &script), State::Fresh);
            }
        }
    }
}
