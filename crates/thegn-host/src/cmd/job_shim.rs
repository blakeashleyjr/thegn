//! `thegn job shim`, `thegn job shim-install` and `thegn job match` — the
//! automatic route for a stock terminal agent's build or test.
//!
//! A PATH-prepended `cargo` wrapper (written by `shim-install`) calls `thegn
//! job shim -- cargo <args>`. When the project opted in (`[project.<slug>.jobs]
//! route = "auto"`), has a runner, and the command is exactly a qualified
//! recipe (`cargo build` or `cargo test`, run at the worktree root), the shim
//! forwards it through the same request `thegn job run` sends. Everything else
//! `exec`s the real tool untouched. Compound (`a && b`) and piped (`a | b`)
//! lines need no parsing at run time: the shell runs each command, and each
//! `cargo` hits the shim. `job match` applies the same rules to a whole shell
//! line so the coverage is inspectable.
//!
//! A qualified command that cannot reach its runner fails closed: the shim
//! never starts a local compiler as a recovery action. Running locally is a
//! separate, visible choice (`THEGN_JOB_LOCAL=1`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use thegn_core::config::{Config, RouteMode};
use thegn_core::outln;
use thegn_svc::selected_job;

use super::job::{DEFAULT_DEADLINE_SECS, RecipeArg, RunArgs};

/// Set by the installed wrapper so the real tool is looked up outside it.
pub const SHIM_DIR_ENV: &str = "THEGN_JOB_SHIM_DIR";
/// An explicit, visible "run this one locally" request.
pub const LOCAL_ENV: &str = "THEGN_JOB_LOCAL";
/// The tools the shim wraps.
const SHIM_TOOLS: &[&str] = &["cargo"];
/// Environment the closed recipe cannot forward: a build that depends on it
/// would differ remotely, so it is not routed.
const BLOCKING_ENV: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC",
    "CARGO_TARGET_DIR",
];
const BLOCKING_ENV_PREFIXES: &[&str] = &["CARGO_PROFILE_", "CARGO_BUILD_"];

#[derive(clap::Args, Clone, Debug)]
pub struct ShimArgs {
    /// The tool and its arguments, exactly as the user typed them
    /// (`-- cargo build`).
    #[arg(required = true, num_args = 1.., allow_hyphen_values = true, value_name = "TOOL ARGS")]
    pub argv: Vec<String>,
}

#[derive(clap::Args, Clone, Debug)]
pub struct ShimInstallArgs {
    /// Directory to hold the wrappers (created if missing). Put it first on
    /// PATH for the agent's terminal.
    #[arg(long, value_name = "DIR")]
    pub dir: PathBuf,
}

#[derive(clap::Args, Clone, Debug)]
pub struct MatchArgs {
    /// Print one JSON document instead of text.
    #[arg(long)]
    pub json: bool,
    /// The shell command line to classify (quote it as one argument).
    #[arg(value_name = "LINE")]
    pub line: String,
}

/// One command of a shell line and where the shim would send it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct StageVerdict {
    pub command: String,
    /// `remote` (a qualified recipe) or `local`.
    pub route: &'static str,
    /// The recipe name when `route` is `remote`.
    pub recipe: Option<&'static str>,
    pub reason: String,
    #[serde(skip)]
    pub recipe_arg: Option<RecipeArg>,
    /// A qualified recipe with arguments the closed recipe cannot forward.
    #[serde(skip)]
    pub near_miss: bool,
}

impl StageVerdict {
    fn local(command: String, reason: impl Into<String>) -> Self {
        Self {
            command,
            route: "local",
            recipe: None,
            reason: reason.into(),
            recipe_arg: None,
            near_miss: false,
        }
    }

    fn remote(command: String, arg: RecipeArg) -> Self {
        let name = selected_job::recipe_name(arg.recipe());
        Self {
            command,
            route: "remote",
            recipe: Some(name),
            reason: format!("qualified recipe {name}"),
            recipe_arg: Some(arg),
            near_miss: false,
        }
    }
}

/// Every command of a shell line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct LineVerdict {
    /// `direct`, `compound`, `piped`, `compound+piped` or `unsupported`.
    pub form: &'static str,
    pub stages: Vec<StageVerdict>,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    And,
    Pipe,
}

/// Split a line into words, `&&` and `|`. Anything the matcher does not model
/// (substitution, redirection, `;`, `||`, background, globs, escapes,
/// newlines) is an error naming it, so the whole line is reported as outside
/// coverage rather than guessed at.
fn tokenize(line: &str) -> std::result::Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars().peekable();
    macro_rules! flush {
        () => {
            if let Some(done) = word.take() {
                tokens.push(Token::Word(done));
            }
        };
    }
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => flush!(),
            '\n' | '\r' => return Err("a multi-line command".into()),
            '\'' => {
                let cur = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err("an unterminated quote".into()),
                    }
                }
            }
            '"' => {
                let cur = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('$' | '`' | '\\') => {
                            return Err("expansion inside double quotes".into());
                        }
                        Some(ch) => cur.push(ch),
                        None => return Err("an unterminated quote".into()),
                    }
                }
            }
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    flush!();
                    tokens.push(Token::And);
                } else {
                    return Err("background execution (`&`)".into());
                }
            }
            '|' => match chars.peek() {
                Some('|') => return Err("`||`".into()),
                Some('&') => return Err("`|&`".into()),
                _ => {
                    flush!();
                    tokens.push(Token::Pipe);
                }
            },
            ';' | '$' | '`' | '(' | ')' | '{' | '}' | '<' | '>' | '\\' | '*' | '?' | '[' | ']'
            | '~' | '#' | '!' => return Err(format!("shell syntax `{c}`")),
            other => word.get_or_insert_with(String::new).push(other),
        }
    }
    flush!();
    Ok(tokens)
}

/// Classify one command's words.
fn classify_words(words: &[String]) -> StageVerdict {
    let command = words.join(" ");
    let Some(tool) = words.first() else {
        return StageVerdict::local(command, "empty command");
    };
    if tool.contains('=') {
        return StageVerdict::local(
            command,
            "an environment assignment before the command is not forwarded",
        );
    }
    if !SHIM_TOOLS.contains(&tool.as_str()) {
        return StageVerdict::local(command, "no recipe covers this command");
    }
    let rest: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    match rest.as_slice() {
        ["build"] => StageVerdict::remote(command, RecipeArg::CargoBuild),
        ["test"] => StageVerdict::remote(command, RecipeArg::CargoTest),
        ["build" | "test", ..] => {
            let mut verdict = StageVerdict::local(
                command,
                "arguments are not forwarded: the recipe is closed (bare `cargo build` or \
                 `cargo test` only)",
            );
            verdict.near_miss = true;
            verdict
        }
        _ => StageVerdict::local(command, "no recipe covers this cargo subcommand"),
    }
}

/// Classify a whole shell line: direct, `a && b`, `a | b` or both.
pub(crate) fn classify_line(line: &str) -> LineVerdict {
    let unsupported = |why: String| LineVerdict {
        form: "unsupported",
        stages: vec![StageVerdict::local(
            line.trim().to_owned(),
            format!("outside selective coverage: {why}"),
        )],
    };
    let tokens = match tokenize(line) {
        Ok(tokens) => tokens,
        Err(why) => return unsupported(why),
    };
    // pipelines (split on `&&`) of stages (split on `|`) of words.
    let mut pipelines: Vec<Vec<Vec<String>>> = vec![vec![Vec::new()]];
    for token in tokens {
        let pipeline = pipelines.last_mut().expect("one pipeline");
        match token {
            Token::Word(word) => pipeline.last_mut().expect("one stage").push(word),
            Token::Pipe => pipeline.push(Vec::new()),
            Token::And => pipelines.push(vec![Vec::new()]),
        }
    }
    if pipelines.iter().flatten().any(Vec::is_empty) {
        return unsupported("an empty command around `&&` or `|`".into());
    }
    let compound = pipelines.len() > 1;
    let piped = pipelines.iter().any(|pipeline| pipeline.len() > 1);
    let form = match (compound, piped) {
        (false, false) => "direct",
        (true, false) => "compound",
        (false, true) => "piped",
        (true, true) => "compound+piped",
    };
    let mut stages = Vec::new();
    for pipeline in &pipelines {
        for (index, words) in pipeline.iter().enumerate() {
            let mut verdict = classify_words(words);
            if index > 0 && verdict.recipe_arg.is_some() {
                // A remote job takes no stdin.
                verdict = StageVerdict::local(
                    verdict.command,
                    "reads stdin from a pipe; a remote job takes no stdin",
                );
            }
            stages.push(verdict);
        }
    }
    LineVerdict { form, stages }
}

/// Everything the shim's decision depends on, read once so the decision is a
/// pure function.
#[derive(Debug, Clone)]
pub(crate) struct ShimContext {
    pub inside_job: bool,
    pub force_local: bool,
    pub mode: RouteMode,
    /// The project's effective runner env; empty = none (stays local).
    pub runner_env: String,
    pub at_project_root: bool,
    /// Set environment variables the recipe cannot forward.
    pub blocking_env: Vec<String>,
}

/// What the shim does with one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Forward this recipe to the project's runner.
    Remote(RecipeArg),
    /// `exec` the real tool; print `notice` (stderr) first when present.
    Local { notice: Option<String> },
}

/// Decide for `argv` (tool first). Silent for commands the shim has no
/// opinion on; a notice whenever a command that looks like a recipe stays
/// local.
pub(crate) fn decide(argv: &[String], ctx: &ShimContext) -> Decision {
    let silent = Decision::Local { notice: None };
    if ctx.inside_job {
        return silent;
    }
    let stage = classify_words(argv);
    if ctx.mode != RouteMode::Auto || ctx.runner_env.is_empty() {
        return silent;
    }
    let notice = |text: String| Decision::Local { notice: Some(text) };
    if stage.near_miss {
        return notice(format!("running locally: {}", stage.reason));
    }
    let Some(recipe) = stage.recipe_arg else {
        return silent;
    };
    if ctx.force_local {
        return notice(format!(
            "running `{}` locally because {LOCAL_ENV} is set",
            stage.command
        ));
    }
    if !ctx.at_project_root {
        return notice(
            "running locally: the recipe runs at the project root and the current directory \
             is elsewhere"
                .into(),
        );
    }
    if !ctx.blocking_env.is_empty() {
        return notice(format!(
            "running locally: {} would change the build and is not forwarded",
            ctx.blocking_env.join(", ")
        ));
    }
    Decision::Remote(recipe)
}

fn is_blocking_env(name: &str) -> bool {
    BLOCKING_ENV.contains(&name)
        || BLOCKING_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v.to_str() != Some("0"))
}

fn context(cfg: &Config, worktree: &Path) -> ShimContext {
    let (mode, runner_env) = match thegn_core::repo::main_worktree(worktree) {
        Some(root) => (cfg.repo_job_route(&root), cfg.repo_job_runner(&root).env),
        None => (RouteMode::Off, String::new()),
    };
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let root = worktree.canonicalize().ok();
    ShimContext {
        inside_job: selected_job::inside_selected_job(),
        force_local: env_flag(LOCAL_ENV),
        mode,
        runner_env,
        at_project_root: cwd.is_some() && cwd == root,
        blocking_env: std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| is_blocking_env(name))
            .collect(),
    }
}

/// `thegn job shim -- <tool> <args…>`.
pub(crate) fn shim(cfg: &Config, args: &ShimArgs) -> Result<()> {
    let Some(tool) = args.argv.first() else {
        bail!("no tool given");
    };
    if !SHIM_TOOLS.contains(&tool.as_str()) {
        bail!(
            "`thegn job shim` wraps only {}; {tool:?} is not shimmed",
            SHIM_TOOLS.join(", ")
        );
    }
    let worktree = super::resolve_worktree(None);
    let ctx = context(cfg, &worktree);
    match decide(&args.argv, &ctx) {
        Decision::Remote(recipe) => {
            thegn_core::msg::info(&format!(
                "routing `{}` to project runner env {}",
                args.argv.join(" "),
                ctx.runner_env
            ));
            let run = RunArgs {
                runner: None,
                deadline: DEFAULT_DEADLINE_SECS,
                recipe,
            };
            super::job::run_recipe(cfg, &run).context(format!(
                "`{}` was routed to a remote runner and did not run locally. Nothing falls back \
                 to a local build; `thegn job route` shows the destination, and \
                 {LOCAL_ENV}=1 runs this one command locally on purpose",
                args.argv.join(" ")
            ))
        }
        Decision::Local { notice } => {
            if let Some(text) = notice {
                thegn_core::msg::info(&text);
            }
            passthrough(&args.argv)
        }
    }
}

fn same_dir(entry: &Path, shim_dir: Option<&Path>) -> bool {
    shim_dir.is_some_and(|shim| {
        entry == shim
            || matches!(
                (entry.canonicalize(), shim.canonicalize()),
                (Ok(a), Ok(b)) if a == b
            )
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file() || path.with_extension("exe").is_file()
}

/// The first executable `tool` in `entries`.
pub(crate) fn find_real_tool(tool: &str, entries: &[PathBuf]) -> Option<PathBuf> {
    entries
        .iter()
        .map(|dir| dir.join(tool))
        .find(|candidate| is_executable(candidate))
}

/// Run the real tool, looked up on PATH without the shim directory (which is
/// also removed from the child's PATH, so a build script that calls `cargo`
/// does not come back through the shim).
fn passthrough(argv: &[String]) -> Result<()> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let shim_dir = std::env::var_os(SHIM_DIR_ENV).map(PathBuf::from);
    let entries: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|entry| !same_dir(entry, shim_dir.as_deref()))
        .collect();
    let real = find_real_tool(&argv[0], &entries)
        .ok_or_else(|| anyhow!("`{}` is not on PATH outside the shim directory", argv[0]))?;
    let new_path = std::env::join_paths(&entries).context("cannot rebuild PATH")?;
    exec_real(&real, &argv[1..], &new_path)
}

#[cfg(unix)]
fn exec_real(real: &Path, args: &[String], path: &std::ffi::OsStr) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // CLI verb, never on the compositor loop: exec replaces this process so
    // the tool owns the terminal and signals.
    let err = std::process::Command::new(real)
        .args(args)
        .env("PATH", path)
        .exec();
    Err(err).with_context(|| format!("cannot run {}", real.display()))
}

#[cfg(not(unix))]
fn exec_real(real: &Path, args: &[String], path: &std::ffi::OsStr) -> Result<()> {
    let status = std::process::Command::new(real)
        .args(args)
        .env("PATH", path)
        .status()
        .with_context(|| format!("cannot run {}", real.display()))?;
    std::process::exit(status.code().unwrap_or(super::EXIT_ERROR));
}

/// Single-quote `text` for a POSIX shell.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The wrapper script for `tool`.
pub(crate) fn wrapper_script(exe: &Path, tool: &str) -> String {
    format!(
        "#!/bin/sh\n# Written by `thegn job shim-install`: routes a qualified `{tool}` recipe to the \
         project's runner.\nexec {} job shim -- {tool} \"$@\"\n",
        sh_quote(&exe.display().to_string())
    )
}

/// Write the wrappers into `dir` (created if missing; rewriting is idempotent)
/// and return its canonical path.
#[cfg(unix)]
pub(crate) fn write_wrappers(dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe().context("cannot locate the thegn binary")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", dir.display()))?;
    for tool in SHIM_TOOLS {
        let path = dir.join(tool);
        std::fs::write(&path, wrapper_script(&exe, tool))
            .with_context(|| format!("cannot write {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot make {} executable", path.display()))?;
    }
    Ok(dir)
}

/// `thegn job shim-install --dir DIR`: write the wrappers and print the
/// shell lines that put them first on PATH (`eval "$(…)"`-able).
#[cfg(unix)]
pub(crate) fn install(args: &ShimInstallArgs) -> Result<()> {
    let dir = write_wrappers(&args.dir)?;
    let quoted = sh_quote(&dir.display().to_string());
    outln!("export {}={}", SHIM_DIR_ENV, quoted);
    outln!("export PATH={}:\"$PATH\"", quoted);
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn install(_args: &ShimInstallArgs) -> Result<()> {
    bail!("the shell shim needs a Unix host")
}

/// The directory thegn manages for automatic-route wrappers.
pub(crate) fn managed_shim_dir() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn/job-shim")
}

/// True when `repo_root`'s project opted into the automatic route AND has a
/// runner bound: the only case an agent pane gets the shim directory.
pub(crate) fn pane_gets_shim(cfg: &Config, repo_root: &Path) -> bool {
    cfg.repo_job_route(repo_root) == RouteMode::Auto
        && !cfg.repo_job_runner(repo_root).env.is_empty()
}

/// Put `shim_dir` first on the pane's `PATH` and record it in
/// `THEGN_JOB_SHIM_DIR`. `PATH` starts from the pane env's own value when it
/// has one (toolchain activation sets it), else from `base_path`.
pub(crate) fn prepend_shim_env(env: &mut Vec<(String, String)>, shim_dir: &Path, base_path: &str) {
    let dir = shim_dir.display().to_string();
    let base = env
        .iter()
        .rev()
        .find(|(key, _)| key == "PATH")
        .map_or_else(|| base_path.to_owned(), |(_, value)| value.clone());
    env.retain(|(key, _)| key != "PATH" && key != SHIM_DIR_ENV);
    let path = if base.is_empty() {
        dir.clone()
    } else {
        format!("{dir}:{base}")
    };
    env.push((SHIM_DIR_ENV.to_owned(), dir));
    env.push(("PATH".to_owned(), path));
}

/// Launch-time hook for a host agent pane: when the project routes
/// automatically and has a runner, install the wrappers under the managed
/// directory and prepend it to the pane's `PATH`. Off by default; a failure to
/// write the wrappers leaves the pane unrouted (the build then runs locally,
/// visibly, as it does today) and is logged.
pub(crate) fn inject_pane_env(cfg: &Config, repo_root: &Path, env: &mut Vec<(String, String)>) {
    if !pane_gets_shim(cfg, repo_root) {
        return;
    }
    #[cfg(unix)]
    match write_wrappers(&managed_shim_dir()) {
        Ok(dir) => {
            let base = std::env::var("PATH").unwrap_or_default();
            prepend_shim_env(env, &dir, &base);
        }
        Err(error) => tracing::warn!(
            target: "thegn::job",
            error = %format!("{error:#}"),
            "automatic route: cannot install the job shim, pane stays unrouted"
        ),
    }
    #[cfg(not(unix))]
    let _ = env;
}

/// `thegn job match`: show the coverage of one shell line.
pub(crate) fn match_line(args: &MatchArgs) -> Result<()> {
    let verdict = classify_line(&args.line);
    if args.json {
        return super::emit_json(&verdict);
    }
    outln!("form: {}", verdict.form);
    for stage in &verdict.stages {
        let target = stage.recipe.unwrap_or("-");
        outln!(
            "{}: {} [{}] — {}",
            stage.route,
            stage.command,
            target,
            stage.reason
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "job_shim_tests.rs"]
mod tests;
