//! The `<TAB>`-time fast path, and the only file in this crate that may touch
//! `clap_complete::{engine, env}` (`clap_complete_is_imported_once` asserts it).
//!
//! ## What this is
//!
//! `thegn completions <shell>` installs a **registration shim** whose body calls
//! back into this binary on every `<TAB>`. So the shell pays nothing at startup,
//! the command structure can never go stale (the shim contains no command
//! names — it asks the binary), and *values* are live: `thegn wt rm <TAB>` lists
//! your worktrees, `thegn open <TAB>` your repos, `thegn attach <TAB>` the
//! daemon's live sessions. The policy behind those values —
//! which slot takes which source, the candidate rules, the budget — is
//! [`thegn_core::completion`]; this file is the wiring.
//!
//! ## The contract (THE-36 design §4)
//!
//! Every `<TAB>` press is a process launch and the user is waiting on it, so:
//!
//! - **Cost nothing when not completing.** [`maybe_complete`] is one
//!   `env::var_os` on the normal launch path.
//! - **Never reach `run_subcommand`.** That path resolves the channel, loads the
//!   layered config, calls `merge_db_hosts` (which opens the DB **read-write**),
//!   installs the log subscriber, emits preset/pipeline warnings, installs the
//!   forge and git handles and publishes the cgroup policy. None of it may
//!   happen on a keypress.
//! - **Never create state.** The DB is read-only with a 50 ms busy timeout (see
//!   `thegn_core::completion::sources`); `run_startup_migration` is skipped, for
//!   the same reason the stdio bridges skip it. Consequence, accepted and
//!   documented: on a state root that has not been migrated yet, `<TAB>` gives
//!   structural completions only until the next real `thegn` run. The one
//!   exception, also deliberate: completing under an explicit
//!   `thegn --profile <name>` calls `profile::reroot`, which `mkdir -p`s that
//!   profile's state dir. Without it a `<TAB>` would read the *shared* DB and
//!   offer another profile's worktrees, which is worse than one empty
//!   directory. The default profile — every completion that does not name one —
//!   creates nothing, and that is what smoke asserts.
//! - **Fail open, always.** Any error, timeout or panic exits 0 having printed
//!   nothing, and the shell falls back to filename completion — exactly today's
//!   behaviour. A `<TAB>` must never print a backtrace, a crash notice, a
//!   `config: unknown key` warning, or an error.
//! - **Own stdout.** One candidate per line in the shell's own protocol.
//!   Nothing else may write to it here, for the same reason the stdio bridges
//!   cannot.
//! - **Lazy sources.** A source pays only for its own inputs, and only when the
//!   slot being completed asks for it. `thegn wt <TAB>` (structure only) touches
//!   neither the DB nor the config.

use std::ffi::OsStr;
use std::sync::OnceLock;

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use clap_complete::env::CompleteEnv;

use thegn_core::completion::sources::CompletionSource as _;
use thegn_core::completion::{CATALOG, Candidate, Deadline, SourceKind, candidate, sources};
use thegn_core::config::Config;

/// The environment variable `CompleteEnv` activates on. This is its default
/// (verified against the vendored `clap_complete/src/env/mod.rs`); named here so
/// the cheap "am I completing?" check does not have to construct anything.
const COMPLETE_VAR: &str = "COMPLETE";

/// Serve a shell completion request and exit, if this process was invoked as
/// one. Costs a single env read otherwise.
///
/// Called from the top of `main()` — after `mem::tune_allocator` and
/// `util::scrub_git_env` (env mutation must stay single-threaded and precede any
/// thread) and before `install_panic_hook` / `report_migration`. See the module
/// doc for why that ordering is load-bearing.
pub fn maybe_complete() {
    // The whole cost on a normal launch.
    if std::env::var_os(COMPLETE_VAR).is_none() {
        return;
    }

    // Diagnostics must not reach the terminal: bash's generated shim does NOT
    // redirect stderr (zsh's does), so a `config: unknown key` warning from the
    // lazy config load would paint over the user's prompt mid-keystroke. This is
    // the existing switch for "do not write diagnostics to stderr" — the WARN+
    // ring still captures them in memory, at no I/O cost. (Not a TUI; the flag
    // names the effect, not the caller.)
    thegn_core::msg::set_tui_active(true);

    // Reroot for `thegn --profile work …` BEFORE any source resolves a path, so
    // completing under a profile reads that profile's DB and config overlay.
    let argv: Vec<String> = std::env::args().collect();
    thegn_core::profile::reroot(profile_from_completion_argv(&argv));

    // A silent hook for the duration: the global one (`install_panic_hook`) is
    // not installed yet on this path, and the default hook prints a backtrace.
    // Restored only on the "not actually completing" return — every other exit
    // is `process::exit`.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(serve));

    match served {
        // Served: stdout already carries the answer.
        Ok(true) => std::process::exit(0),
        // `COMPLETE` was set but empty or `0` — clap's documented "disabled"
        // spelling. Carry on with a normal launch.
        Ok(false) => {
            std::panic::set_hook(previous);
            thegn_core::msg::set_tui_active(false);
        }
        // Fail open: nothing printed, exit 0, shell falls back to filenames.
        Err(_) => std::process::exit(0),
    }
}

/// Write the registration shim for `shell` into `buf` — the body of
/// `thegn completions <shell>`.
///
/// This is what a packager installs. It contains no command names: it registers
/// a shell function that re-invokes `bin` with `COMPLETE=<shell>` set on every
/// `<TAB>`, which is what makes the structure unstaleable and the values live.
/// `bin` is the plain command name (`thegn` / `tg`), for the same
/// resolve-through-PATH reason [`serve`] explains.
///
/// A shell `clap_complete` has no dynamic adapter for writes nothing; the caller
/// surfaces that as an empty script, and `--static` remains the answer for it.
pub fn write_registration(shell: clap_complete::Shell, bin: &str, buf: &mut Vec<u8>) {
    use clap_complete::env::Shells;
    let shells = Shells::builtins();
    let Some(completer) = shells.completer(&shell.to_string()) else {
        return;
    };
    // `bin` is passed as the shell-function NAME as well as the command, so the
    // `thegn` and `tg` scripts define distinct functions and can both be
    // installed (a packager installs both — see nix/package.nix).
    // best-effort: `buf` is a Vec, so this cannot actually fail; the write to
    // stdout (and its broken-pipe handling) is the caller's.
    let _ = completer.write_registration(COMPLETE_VAR, bin, bin, bin, buf);
}

/// Run the completion engine. `Ok(true)` = a request was served.
///
/// Split out of [`maybe_complete`] so the `catch_unwind` boundary wraps
/// everything including tree construction, which is where a bad catalog row
/// would blow up.
fn serve() -> bool {
    let deadline = Deadline::from_env();
    // The name this binary was invoked as (`thegn` or the short `tg`), so the
    // shim's registration, the completer it calls back into, and the tree's own
    // name all agree — that is what lets one binary serve both names with no
    // special casing.
    //
    // `completer` is the plain NAME, not a path. It defaults to `args_os()[0]`,
    // which is absolute — and the shipped scripts are generated inside a Nix
    // build sandbox or a CI temp dir, so baking that in would ship a script
    // that calls a path no user has. The name resolves through PATH, which is
    // the only way the user could have typed the command in the first place.
    let bin = bin_name();
    CompleteEnv::with_factory(move || tree(deadline))
        .bin(bin.clone())
        .completer(bin)
        .try_complete(std::env::args_os(), std::env::current_dir().ok().as_deref())
        // Fail open: a bad `COMPLETE=<shell>` value, an unwritable stdout, a
        // malformed command line — all of them complete nothing, silently.
        .unwrap_or(false)
}

/// The command tree a `<TAB>` is answered from: the same grouped tree the parser
/// uses, with the catalog's completers attached.
///
/// Deliberately NOT renamed per alias. The tree's own name reaches only
/// `CompleteEnv`'s registration fallback (`COMPLETE=zsh thegn` with no words),
/// which is not a shipped path — every installed script comes from
/// [`write_registration`], which names its function from `argv[0]`. Completion
/// itself never consults the root name: the engine skips the command word of
/// the line it is completing.
fn tree(deadline: Deadline) -> clap::Command {
    // Decorate BEFORE `cli_help::attach`, which calls `Command::build()`.
    // `Command::mut_arg` removes the arg from the key map and pushes it back at
    // the end; on a *built* command that leaves the long/short index pointing at
    // the wrong slot, and `thegn --profile work wt rm x` starts resolving to
    // `--version`. Mutating an unbuilt tree is the supported order.
    let base = <crate::Cli as clap::CommandFactory>::command();
    crate::cli_help::attach(decorate(base, deadline))
}

/// Attach a value completer to every implemented slot in [`CATALOG`].
///
/// Deliberately NOT `add = …` on the derive in `main.rs`: keeping the binding in
/// the catalog is what makes the drift test meaningful, and decorating the
/// already-built tree is the pattern `cli_help::attach` already established.
fn decorate(mut cmd: clap::Command, deadline: Deadline) -> clap::Command {
    for slot in CATALOG {
        if !slot.source.is_implemented() {
            continue;
        }
        let path: Vec<&str> = slot.command_path.split_whitespace().collect();
        cmd = attach_at(cmd, &path, slot.arg_id, slot.source, deadline);
    }
    cmd
}

/// Walk to `path` and attach a completer to `arg_id`.
///
/// A path or arg the tree does not have is skipped rather than panicking:
/// `completion_slots_are_bound_or_pinned` is the gate that keeps the catalog
/// honest, and a `<TAB>` is the wrong place to enforce it.
fn attach_at(
    cmd: clap::Command,
    path: &[&str],
    arg_id: &str,
    kind: SourceKind,
    deadline: Deadline,
) -> clap::Command {
    match path.split_first() {
        Some((head, rest)) => {
            if cmd.find_subcommand(head).is_none() {
                return cmd;
            }
            cmd.mut_subcommand(*head, |c| attach_at(c, rest, arg_id, kind, deadline))
        }
        None => {
            if !cmd.get_arguments().any(|a| a.get_id() == arg_id) {
                return cmd;
            }
            let completer =
                ArgValueCompleter::new(move |current: &OsStr| wire(kind, current, &deadline));
            // `mut_args`, NOT the singular `mut_arg`: the latter removes the arg
            // and pushes it back at the *end* of the list, and clap numbers
            // positionals by list order at build time. Decorating `env set`'s
            // optional `worktree_pos` before its required `name` would therefore
            // swap their indices — which clap's own debug assert catches as
            // "non-required positional with a lower index than a required one".
            // `mut_args` maps in place and cannot reorder anything.
            cmd.mut_args(move |a| {
                if a.get_id() == arg_id {
                    a.add(completer.clone())
                } else {
                    a
                }
            })
        }
    }
}

/// One slot's answer, in `clap_complete`'s candidate type.
fn wire(kind: SourceKind, current: &OsStr, deadline: &Deadline) -> Vec<CompletionCandidate> {
    // A non-UTF-8 partial word cannot prefix-match any value we serve; completing
    // nothing is the honest answer.
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    candidates_for(kind, current, deadline)
        .into_iter()
        .map(|c| {
            let candidate = CompletionCandidate::new(c.value);
            match c.description {
                Some(d) => candidate.help(Some(d.into())),
                None => candidate,
            }
        })
        .collect()
}

/// Resolve `kind` to its source, run it, and apply the candidate policy.
///
/// This is the lazy dispatch the contract asks for: a DB-derived slot opens the
/// DB, a config-derived slot loads the config (once per request, via
/// [`config`]), and an in-process slot pays for neither.
fn candidates_for(kind: SourceKind, current: &str, deadline: &Deadline) -> Vec<Candidate> {
    let raw = if kind.reads_db() {
        sources::DbSource::new(kind).candidates(current, deadline)
    } else if kind.reads_config() {
        match config() {
            Some(cfg) => sources::ConfigSource::new(kind, cfg).candidates(current, deadline),
            None => Vec::new(),
        }
    } else if kind.is_implemented() {
        sources::StaticSource::new(kind).candidates(current, deadline)
    } else {
        Vec::new()
    };
    candidate::refine(raw, current)
}

/// The layered config, loaded at most once per request and only if a
/// config-derived slot is reached.
///
/// **Config only**: no `merge_db_hosts` (it opens the DB read-write), no
/// `clamp_to_channel`, no forge/git handle install, no cgroup publish. Warnings
/// are already routed away from stderr by [`maybe_complete`].
///
/// `--config <path>` is deliberately not honoured here: reading it would mean
/// parsing our own argv for a flag whose only effect on a `<TAB>` is which
/// `[[agents]]` names appear. The default layering is what the user's shell
/// sees.
fn config() -> Option<&'static Config> {
    static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            std::panic::catch_unwind(|| {
                Config::load_layered(&thegn_core::config::ProcessEnv, &[], None)
            })
            .ok()
        })
        .as_ref()
}

/// `argv[0]`'s file name, defaulting to `thegn`.
fn bin_name() -> String {
    std::env::args_os()
        .next()
        .and_then(|p| {
            std::path::Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "thegn".into())
}

/// The profile named on the command line being completed.
///
/// `clap_complete` invokes us as `thegn -- <the user's words…>`, so the
/// `--profile` that matters is in the words *after* the `--`, not in our own
/// argv. Falls back to scanning the whole argv, which is the shape a plain
/// `COMPLETE=zsh thegn` registration request has.
fn profile_from_completion_argv(argv: &[String]) -> Option<&str> {
    let after = argv
        .iter()
        .position(|a| a == "--")
        .map(|i| &argv[i + 1..])
        .unwrap_or(&[]);
    profile_from_argv(after).or_else(|| profile_from_argv(argv))
}

/// Scan a command line for `--profile <name>` / `--profile=<name>`.
///
/// Pure and deliberately dumb: stops at a `--` terminator, ignores a trailing
/// `--profile` with no value, and treats an empty value as absent (which is what
/// `profile::reroot` does with one anyway).
fn profile_from_argv(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            return None;
        }
        let value = if let Some(v) = arg.strip_prefix("--profile=") {
            v
        } else if arg == "--profile" {
            it.next()?.as_str()
        } else {
            continue;
        };
        return (!value.trim().is_empty()).then_some(value);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_string()).collect()
    }

    // ── the `--profile` argv scan ────────────────────────────────────────────

    #[test]
    fn profile_scan_handles_both_spellings_and_the_edges() {
        let cases: &[(&[&str], Option<&str>)] = &[
            (&["thegn", "wt", "list"], None),
            (&["thegn", "--profile", "work", "wt", "list"], Some("work")),
            (&["thegn", "--profile=work", "wt", "list"], Some("work")),
            // Trailing `--profile` with no value: not a panic, not a guess.
            (&["thegn", "--profile"], None),
            // Empty value == absent (reroot treats it that way too).
            (&["thegn", "--profile="], None),
            (&["thegn", "--profile", "  "], None),
            // Past a `--` terminator it is a positional, not our flag.
            (&["thegn", "--", "--profile", "work"], None),
            // A longer flag that merely starts the same way is not a match.
            (&["thegn", "--profiles", "work"], None),
            // First occurrence wins.
            (&["thegn", "--profile", "a", "--profile", "b"], Some("a")),
        ];
        for (words, want) in cases {
            assert_eq!(profile_from_argv(&args(words)), *want, "argv {words:?}");
        }
    }

    #[test]
    fn profile_scan_reads_the_completed_command_line_not_ours() {
        // The shape clap_complete actually invokes: our argv, `--`, then the
        // words the user has typed. The user's `--profile` is what counts.
        let argv = args(&[
            "/nix/store/…/bin/thegn",
            "--",
            "thegn",
            "--profile",
            "work",
            "wt",
            "rm",
            "",
        ]);
        assert_eq!(profile_from_completion_argv(&argv), Some("work"));

        // A registration request (no `--`) still honours our own argv.
        let argv = args(&["thegn", "--profile=work"]);
        assert_eq!(profile_from_completion_argv(&argv), Some("work"));

        // Neither side names one.
        let argv = args(&["thegn", "--", "thegn", "wt", "rm", ""]);
        assert_eq!(profile_from_completion_argv(&argv), None);
    }

    #[test]
    fn bin_name_is_never_empty() {
        // Whatever the test runner is called, the fallback keeps this usable as
        // a clap command name.
        let name = bin_name();
        assert!(!name.is_empty());
        assert!(!name.contains('/'));
    }

    // ── the decorated tree ───────────────────────────────────────────────────

    fn built() -> clap::Command {
        let mut cmd = crate::cli_help::attach(<crate::Cli as clap::CommandFactory>::command());
        cmd.build();
        cmd
    }

    fn decorated() -> clap::Command {
        let mut cmd = tree(Deadline::new(1_000));
        cmd.build();
        cmd
    }

    #[test]
    fn decoration_does_not_change_parsing() {
        // Decoration adds an `ArgExt`; it must not move an arg, change a
        // default, or alter how a command line resolves.
        let line = ["thegn", "--profile", "work", "wt", "rm", "/wt/x", "--force"];
        let plain = built().try_get_matches_from(line).expect("plain parses");
        let deco = decorated()
            .try_get_matches_from(line)
            .expect("decorated parses");
        for m in [&plain, &deco] {
            let (name, sub) = m.subcommand().expect("wt");
            assert_eq!(name, "wt");
            let (name, sub) = sub.subcommand().expect("rm");
            assert_eq!(name, "rm");
            assert_eq!(sub.get_one::<String>("target").unwrap(), "/wt/x");
            assert!(sub.get_flag("force"));
        }
        assert_eq!(
            plain.get_one::<String>("profile"),
            deco.get_one::<String>("profile")
        );

        // The same set of commands and args, before and after.
        assert_eq!(slots(&built()), slots(&decorated()));

        // And a bad command line still fails the same way.
        assert!(
            decorated()
                .try_get_matches_from(["thegn", "no-such-verb"])
                .is_err()
        );
    }

    #[test]
    fn every_implemented_catalog_slot_actually_binds() {
        // `attach_at` skips a path/arg the tree does not have, so the catalog
        // could silently name a slot that no longer exists. The drift test below
        // catches a *new* unbound arg; this catches a *stale* catalog row.
        let all = slots(&built());
        for slot in CATALOG {
            assert!(
                all.contains(&(slot.command_path.to_string(), slot.arg_id.to_string())),
                "catalog row {:?} {:?} names a slot the CLI tree does not have",
                slot.command_path,
                slot.arg_id
            );
        }
    }

    // ── the slot-drift ratchet ───────────────────────────────────────────────

    fn ratchet_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test/completion-slot-ratchet.txt")
    }

    fn ratchet() -> BTreeSet<String> {
        std::fs::read_to_string(ratchet_path())
            .unwrap_or_default()
            .lines()
            // Trailing whitespace only: a root-level argument's line *starts*
            // with the tab separator (its command path is empty), so trimming
            // both sides would silently mangle it into a nonexistent key.
            .map(str::trim_end)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect()
    }

    /// Every `(command path, arg id)` in the tree that takes a value.
    fn slots(cmd: &clap::Command) -> BTreeSet<(String, String)> {
        let mut out = BTreeSet::new();
        collect_slots(cmd, "", &mut out);
        out
    }

    fn collect_slots(cmd: &clap::Command, path: &str, out: &mut BTreeSet<(String, String)>) {
        for arg in cmd.get_arguments() {
            let takes_values = arg
                .get_num_args()
                .map(|r| r.takes_values())
                .unwrap_or(false);
            // `global = true` args (`--config`, `--set`, `--profile`,
            // `--log-level`) are propagated by clap into every subcommand, so
            // counting them per path would be ~250 identical rows each — four
            // decisions dressed up as a thousand. They are classified once, at
            // the root; `attach_at` binds them there and clap propagates the
            // `ArgExt` with the rest of the arg.
            let below_root_global = arg.is_global_set() && !path.is_empty();
            if takes_values && !below_root_global {
                out.insert((path.to_string(), arg.get_id().to_string()));
            }
        }
        for sub in cmd.get_subcommands() {
            // clap's own `help` subcommand is rendered by clap, not by us.
            if sub.get_name() == "help" {
                continue;
            }
            let child = if path.is_empty() {
                sub.get_name().to_string()
            } else {
                format!("{path} {}", sub.get_name())
            };
            collect_slots(sub, &child, out);
        }
    }

    fn key(path: &str, arg: &str) -> String {
        format!("{path}\t{arg}")
    }

    /// Every value-taking argument in the live clap tree is either bound to a
    /// source in `CATALOG` or pinned in `test/completion-slot-ratchet.txt`.
    /// The allowlist may only shrink.
    #[test]
    fn completion_slots_are_bound_or_pinned() {
        let live = slots(&built());
        let allow = ratchet();
        let bound: BTreeSet<String> = CATALOG
            .iter()
            .map(|s| key(s.command_path, s.arg_id))
            .collect();

        let mut unclassified: Vec<String> = Vec::new();
        let mut now_bound: Vec<String> = Vec::new();
        for (path, arg) in &live {
            let k = key(path, arg);
            match (bound.contains(&k), allow.contains(&k)) {
                (false, false) => unclassified.push(format!("{path:?} {arg:?}")),
                (true, true) => now_bound.push(format!("{path:?} {arg:?}")),
                _ => {}
            }
        }
        assert!(
            unclassified.is_empty(),
            "new value-taking argument(s) with no completion source: {unclassified:#?}\n\
             Classify each in `thegn_core::completion::CATALOG` — including as \
             `SourceKind::Structural` when clap already completes it from the tree.\n\
             Do NOT add to test/completion-slot-ratchet.txt — the allowlist only shrinks."
        );
        assert!(
            now_bound.is_empty(),
            "slot(s) now in CATALOG but still allowlisted: {now_bound:#?}\n\
             Delete those lines from test/completion-slot-ratchet.txt to lock in the win."
        );

        // No stale lines: a pinned slot that no longer exists is dead debt.
        let live_keys: BTreeSet<String> = live.iter().map(|(p, a)| key(p, a)).collect();
        let stale: Vec<&String> = allow.difference(&live_keys).collect();
        assert!(
            stale.is_empty(),
            "test/completion-slot-ratchet.txt pins slot(s) the CLI no longer has: {stale:#?}\n\
             Delete the stale lines."
        );
    }

    /// Regenerate the allowlist after paying debt down. Run with:
    /// `cargo nextest run -p thegn-host --run-ignored all update_completion_slot_ratchet`
    #[test]
    #[ignore = "writes test/completion-slot-ratchet.txt"]
    fn update_completion_slot_ratchet() {
        let bound: BTreeSet<String> = CATALOG
            .iter()
            .map(|s| key(s.command_path, s.arg_id))
            .collect();
        let mut lines: Vec<String> = slots(&built())
            .iter()
            .map(|(p, a)| key(p, a))
            .filter(|k| !bound.contains(k))
            .collect();
        lines.sort();
        let header = "\
# completion-slot-ratchet — value-taking arguments in the clap tree that no
# `thegn_core::completion::CATALOG` row classifies yet, so a `<TAB>` on them
# completes filenames.
#
# One slot per line, as `<command path>\\t<arg id>` — so a line for a
# ROOT-LEVEL argument starts with the tab. An entry means: this argument
# takes a value, and nobody
# has decided where those values come from — DB, config, in-process, clap's own
# structural completion (`SourceKind::Structural`), or `Reserved` with a reason.
#
# This list may only SHRINK: classify a slot in CATALOG and delete its line
# (or regenerate with the `update_completion_slot_ratchet` test). A NEW
# value-taking argument must be classified immediately — the drift test refuses
# additions here.\n";
        std::fs::write(ratchet_path(), format!("{header}{}\n", lines.join("\n")))
            .expect("write completion-slot-ratchet.txt");
    }

    // ── containment ──────────────────────────────────────────────────────────

    /// The unstable `clap_complete` surface is confined to this file. Same
    /// spirit as `test/forge-leak-ratchet.txt`, but a plain assertion: there is
    /// no debt to pin, so there is no allowlist to keep.
    #[test]
    fn clap_complete_is_imported_once() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits: Vec<String> = Vec::new();
        walk(&src, &mut |path, body| {
            if body.contains("clap_complete::engine") || body.contains("clap_complete::env") {
                hits.push(
                    path.strip_prefix(&src)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        });
        assert_eq!(
            hits,
            ["complete.rs"],
            "`clap_complete::{{engine,env}}` are explicitly-unstable APIs and must \
             stay confined to src/complete.rs so a clap bump breaks exactly one file"
        );
    }

    fn walk(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path, &str)) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                walk(&path, f);
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Ok(body) = std::fs::read_to_string(&path)
            {
                f(&path, &body);
            }
        }
    }

    // ── candidate wiring ─────────────────────────────────────────────────────

    #[test]
    fn wire_maps_descriptions_and_survives_a_non_utf8_word() {
        let dl = Deadline::new(1_000);
        let caps = wire(SourceKind::Capability, OsStr::new("sessions."), &dl);
        assert!(!caps.is_empty());
        assert!(caps.iter().all(|c| c.get_help().is_some()));
        assert!(
            caps.iter()
                .all(|c| c.get_value().to_string_lossy().starts_with("sessions."))
        );

        // Nothing on this path may panic on a partial word the shell hands us
        // verbatim.
        let themes = wire(SourceKind::Theme, OsStr::new(""), &dl);
        assert!(themes.iter().any(|c| c.get_value() == "prism"));
        assert!(themes.iter().all(|c| c.get_help().is_none()));

        // A reserved/structural kind completes nothing rather than erroring.
        assert!(wire(SourceKind::Structural, OsStr::new(""), &dl).is_empty());
    }

    #[test]
    fn an_expired_budget_completes_nothing_anywhere() {
        let spent = Deadline::starting_at(
            std::time::Instant::now() - std::time::Duration::from_secs(1),
            1,
        );
        for kind in [
            SourceKind::Capability,
            SourceKind::Theme,
            SourceKind::Action,
            SourceKind::Worktree,
        ] {
            assert!(
                candidates_for(kind, "", &spent).is_empty(),
                "{kind:?} ignored the deadline"
            );
        }
    }
}
