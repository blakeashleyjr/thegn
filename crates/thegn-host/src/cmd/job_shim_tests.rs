use super::*;
use clap::Parser;

fn parse(args: &[&str]) -> Result<crate::Cli, clap::Error> {
    crate::Cli::try_parse_from(std::iter::once("thegn").chain(args.iter().copied()))
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn routes(line: &str) -> Vec<(&'static str, Option<&'static str>)> {
    classify_line(line)
        .stages
        .iter()
        .map(|stage| (stage.route, stage.recipe))
        .collect()
}

fn ready() -> ShimContext {
    ShimContext {
        inside_job: false,
        force_local: false,
        mode: RouteMode::Auto,
        runner_env: "sp".into(),
        at_project_root: true,
        blocking_env: Vec::new(),
    }
}

#[test]
fn a_direct_recipe_is_remote() {
    let verdict = classify_line("cargo build");
    assert_eq!(verdict.form, "direct");
    assert_eq!(routes("cargo build"), [("remote", Some("cargo-build"))]);
    assert_eq!(routes("  cargo   test "), [("remote", Some("cargo-test"))]);
    assert_eq!(routes("'cargo' \"test\""), [("remote", Some("cargo-test"))]);
}

#[test]
fn a_compound_line_routes_each_qualified_command() {
    let verdict = classify_line("cargo build && cargo test && git status");
    assert_eq!(verdict.form, "compound");
    assert_eq!(
        routes("cargo build && cargo test && git status"),
        [
            ("remote", Some("cargo-build")),
            ("remote", Some("cargo-test")),
            ("local", None)
        ]
    );
}

#[test]
fn a_piped_recipe_routes_only_the_producer() {
    let verdict = classify_line("cargo test | tail -n 20");
    assert_eq!(verdict.form, "piped");
    assert_eq!(
        routes("cargo test | tail -n 20"),
        [("remote", Some("cargo-test")), ("local", None)]
    );
    // A recipe that reads a pipe would need stdin remotely.
    let consumer = classify_line("yes | cargo build");
    assert_eq!(consumer.stages[1].route, "local");
    assert!(consumer.stages[1].reason.contains("stdin"));
    assert_eq!(
        classify_line("cargo build && cargo test | tail").form,
        "compound+piped"
    );
}

#[test]
fn arguments_the_closed_recipe_cannot_forward_stay_local() {
    for line in [
        "cargo build --release",
        "cargo test -p thegn-core",
        "cargo test foo",
    ] {
        let verdict = classify_line(line);
        assert_eq!(verdict.stages[0].route, "local", "{line}");
        assert!(verdict.stages[0].near_miss, "{line}");
        assert!(verdict.stages[0].reason.contains("closed"), "{line}");
    }
    for line in [
        "cargo fmt",
        "cargo +nightly build",
        "cargo",
        "ls",
        "make test",
    ] {
        let verdict = classify_line(line);
        assert_eq!(verdict.stages[0].route, "local", "{line}");
        assert!(!verdict.stages[0].near_miss, "{line}");
    }
}

#[test]
fn syntax_outside_coverage_is_named_not_guessed() {
    for line in [
        "cargo build; cargo test",
        "cargo build || true",
        "cargo build &",
        "cargo build > out.log",
        "cargo build 2>&1",
        "(cargo build)",
        "cargo build $(echo)",
        "cargo \"$X\"",
        "cargo build `x`",
        "cargo build\ncargo test",
        "&& cargo build",
        "cargo build |",
        "cargo 'build",
        "",
        "FOO=1 cargo build",
    ] {
        let verdict = classify_line(line);
        assert!(
            verdict.stages.iter().all(|stage| stage.route == "local"),
            "{line:?}: {verdict:?}"
        );
    }
    let verdict = classify_line("cargo build; cargo test");
    assert_eq!(verdict.form, "unsupported");
    assert!(
        verdict.stages[0]
            .reason
            .contains("outside selective coverage")
    );
    assert!(
        classify_line("FOO=1 cargo build").stages[0]
            .reason
            .contains("environment assignment")
    );
}

#[test]
fn quoted_operators_are_data_not_syntax() {
    let verdict = classify_line("echo 'a && b | c'");
    assert_eq!(verdict.form, "direct");
    assert_eq!(verdict.stages.len(), 1);
}

#[test]
fn the_decision_is_remote_only_for_an_opted_in_project() {
    let build = argv(&["cargo", "build"]);
    assert_eq!(
        decide(&build, &ready()),
        Decision::Remote(RecipeArg::CargoBuild)
    );
    let off = ShimContext {
        mode: RouteMode::Off,
        ..ready()
    };
    assert_eq!(decide(&build, &off), Decision::Local { notice: None });
    let no_runner = ShimContext {
        runner_env: String::new(),
        ..ready()
    };
    assert_eq!(decide(&build, &no_runner), Decision::Local { notice: None });
}

#[test]
fn unrelated_commands_stay_local_without_noise() {
    for words in [&["cargo", "fmt"][..], &["cargo", "--version"][..]] {
        assert_eq!(
            decide(&argv(words), &ready()),
            Decision::Local { notice: None }
        );
    }
}

#[test]
fn a_disqualified_recipe_runs_locally_and_says_why() {
    let build = argv(&["cargo", "build"]);
    let cases = [
        (
            ShimContext {
                at_project_root: false,
                ..ready()
            },
            "project root",
        ),
        (
            ShimContext {
                blocking_env: vec!["RUSTFLAGS".into()],
                ..ready()
            },
            "RUSTFLAGS",
        ),
        (
            ShimContext {
                force_local: true,
                ..ready()
            },
            LOCAL_ENV,
        ),
    ];
    for (ctx, needle) in cases {
        match decide(&build, &ctx) {
            Decision::Local {
                notice: Some(text), ..
            } => assert!(text.contains(needle), "{text}"),
            other => panic!("expected a local notice, got {other:?}"),
        }
    }
    match decide(&argv(&["cargo", "build", "--release"]), &ready()) {
        Decision::Local {
            notice: Some(text), ..
        } => assert!(text.contains("closed"), "{text}"),
        other => panic!("expected a local notice, got {other:?}"),
    }
}

#[test]
fn a_recipe_never_recurses_inside_a_selected_job() {
    let inside = ShimContext {
        inside_job: true,
        ..ready()
    };
    assert_eq!(
        decide(&argv(&["cargo", "build"]), &inside),
        Decision::Local { notice: None }
    );
}

#[test]
fn blocking_environment_names_are_recognised() {
    for name in [
        "RUSTFLAGS",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_JOBS",
        "CARGO_PROFILE_DEV_DEBUG",
    ] {
        assert!(is_blocking_env(name), "{name}");
    }
    for name in ["PATH", "HOME", "CARGO_HOME", "RUST_BACKTRACE"] {
        assert!(!is_blocking_env(name), "{name}");
    }
}

#[test]
fn the_real_tool_is_found_outside_the_shim_directory() {
    let shim = tempfile::tempdir().unwrap();
    let real = tempfile::tempdir().unwrap();
    for dir in [shim.path(), real.path()] {
        let file = dir.join("cargo");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        crate::platform::make_shim_executable(&file).unwrap();
    }
    let entries: Vec<PathBuf> = [shim.path(), real.path()]
        .into_iter()
        .map(Path::to_path_buf)
        .filter(|entry| !same_dir(entry, Some(shim.path())))
        .collect();
    assert_eq!(entries, [real.path().to_path_buf()]);
    assert_eq!(
        find_real_tool("cargo", &entries),
        Some(real.path().join("cargo"))
    );
    assert_eq!(find_real_tool("rustc", &entries), None);
}

#[test]
fn the_wrapper_execs_the_shim_with_a_quoted_binary_path() {
    let script = wrapper_script(Path::new("/opt/it's/thegn"), "cargo");
    assert!(script.starts_with("#!/bin/sh\n"));
    assert!(
        script.contains("exec '/opt/it'\\''s/thegn' job shim -- cargo \"$@\"\n"),
        "{script}"
    );
}

#[test]
fn only_wrapped_tools_are_accepted_by_the_shim_verb() {
    let cfg = Config::default();
    let args = ShimArgs {
        argv: argv(&["make", "test"]),
    };
    let error = shim(&cfg, &args).unwrap_err().to_string();
    assert!(error.contains("not shimmed"), "{error}");
}

#[test]
fn the_shim_verbs_parse() {
    match parse(&["job", "shim", "--", "cargo", "build", "--release"])
        .unwrap()
        .command
    {
        Some(crate::Command::Job {
            action: super::super::job::Action::Shim(args),
        }) => assert_eq!(args.argv, argv(&["cargo", "build", "--release"])),
        _ => panic!("not a job shim"),
    }
    match parse(&["job", "match", "--json", "cargo build && cargo test"])
        .unwrap()
        .command
    {
        Some(crate::Command::Job {
            action: super::super::job::Action::Match(args),
        }) => {
            assert!(args.json);
            assert_eq!(args.line, "cargo build && cargo test");
        }
        _ => panic!("not a job match"),
    }
    assert!(parse(&["job", "shim"]).is_err());
    assert!(parse(&["job", "shim-install"]).is_err());
}

#[test]
fn a_pane_gets_no_shim_unless_the_project_routes_automatically() {
    let cfg = Config::default();
    assert!(!pane_gets_shim(&cfg, Path::new("/repo")));
    let mut env = vec![("PATH".to_owned(), "/bin".to_owned())];
    inject_pane_env(&cfg, Path::new("/repo"), &mut env);
    assert_eq!(env, vec![("PATH".to_owned(), "/bin".to_owned())]);
}

#[test]
fn the_shim_directory_goes_first_on_the_pane_path() {
    let mut env = vec![
        ("LANG".to_owned(), "C".to_owned()),
        ("PATH".to_owned(), "/act/bin:/bin".to_owned()),
    ];
    prepend_shim_env(&mut env, Path::new("/state/job-shim"), "/ignored");
    let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    assert_eq!(get("PATH"), Some("/state/job-shim:/act/bin:/bin"));
    assert_eq!(get(SHIM_DIR_ENV), Some("/state/job-shim"));
    assert_eq!(get("LANG"), Some("C"));
    // Composing twice does not stack or duplicate the directory variable.
    prepend_shim_env(&mut env, Path::new("/state/job-shim"), "/ignored");
    assert_eq!(env.iter().filter(|(k, _)| k == "PATH").count(), 1);
    assert_eq!(env.iter().filter(|(k, _)| k == SHIM_DIR_ENV).count(), 1);
}

#[test]
fn the_pane_path_falls_back_to_the_base_when_the_env_has_none() {
    let mut env = Vec::new();
    prepend_shim_env(&mut env, Path::new("/s"), "/usr/bin");
    assert!(env.contains(&("PATH".to_owned(), "/s:/usr/bin".to_owned())));
}

#[test]
fn writing_the_wrappers_twice_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("shim");
    let first = write_wrappers(&target).unwrap();
    let second = write_wrappers(&target).unwrap();
    assert_eq!(first, second);
    assert!(is_executable(&first.join("cargo")));
}
