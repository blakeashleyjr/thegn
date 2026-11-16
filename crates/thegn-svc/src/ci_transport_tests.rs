//! THE-541 fake-transport proof: host/account separation and secret-free
//! command and error surfaces for the CI providers. Everything here uses a
//! fake `glab` placed on PATH (the same seam as the `ci.rs` bound-client
//! tests), so nothing touches the network.

use super::*;

const HOST_A: &str = "gitlab-a.example.test";
const HOST_B: &str = "gitlab-b.example.test";
const TOKEN_A: &str = "transport-token-a-7f3c";
const TOKEN_B: &str = "transport-token-b-91de";

/// One fake `glab` plus a call log. `body` is the script text run after the
/// log line is appended; it must end with a newline.
struct FakeGlab {
    dir: tempfile::TempDir,
}

impl FakeGlab {
    fn new(body: &str) -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let glab = bin.join("glab");
        let log = dir.path().join("calls.log");
        // Fields: host | token-export marker | token value | argv.
        std::fs::write(
            &glab,
            format!(
                "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$GITLAB_HOST\" \"${{GITLAB_TOKEN+set}}${{GH_TOKEN+set}}\" \"$GITLAB_TOKEN\" \"$*\" >> '{}'\n{body}",
                log.display()
            ),
        )
        .unwrap();
        if thegn_core::fsperm::make_executable_for_test(&glab).is_err() {
            return None;
        }
        Some(Self { dir })
    }

    fn path_guard(&self, extra: &[(&str, Option<&str>)]) -> thegn_core::testenv::EnvGuard {
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut vars: Vec<(&str, Option<&str>)> = vec![("PATH", Some(path.as_str()))];
        vars.extend_from_slice(extra);
        thegn_core::testenv::EnvGuard::mutate_pairs(&vars)
    }

    /// Logged calls as `(host, marker, token, argv)`.
    fn calls(&self) -> Vec<(String, String, String, String)> {
        let text = std::fs::read_to_string(self.dir.path().join("calls.log")).unwrap_or_default();
        text.lines()
            .map(|line| {
                let mut parts = line.splitn(4, '|');
                let mut next = || parts.next().unwrap_or_default().to_string();
                (next(), next(), next(), next())
            })
            .collect()
    }

    fn reset_log(&self) {
        std::fs::write(self.dir.path().join("calls.log"), "").unwrap();
    }
}

fn repo_with_origin(origin: &str) -> (tempfile::TempDir, GitLoc) {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        thegn_core::util::git_cmd(repo.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        thegn_core::util::git_cmd(repo.path())
            .args(["remote", "add", "origin", origin])
            .status()
            .unwrap()
            .success()
    );
    let loc = GitLoc::Local(repo.path().to_path_buf());
    (repo, loc)
}

fn gitlab_cfg(token: &str) -> CiConfig {
    let mut cfg = CiConfig {
        provider: CiProviderKind::Gitlab,
        ..CiConfig::default()
    };
    cfg.gitlab.token = token.into();
    cfg
}

fn non_local_locations() -> Vec<(&'static str, GitLoc)> {
    vec![
        (
            "Remote",
            GitLoc::Remote {
                ssh: thegn_core::remote::SshTarget::plain("build.example.test".into(), 22, false),
                path: "/workspace".into(),
            },
        ),
        (
            "Provider",
            GitLoc::Provider {
                control_prefix: vec![
                    "sprite".into(),
                    "exec".into(),
                    "-s".into(),
                    "fixture".into(),
                    "--".into(),
                ],
                path: "/workspace".into(),
            },
        ),
    ]
}

fn env_keys(cmd: &Command) -> Vec<String> {
    cmd.get_envs()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect()
}

fn argv_text(cmd: &Command) -> String {
    let mut text = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text
}

#[test]
fn transport_non_local_commands_never_export_a_token_and_use_the_remote_login_form() {
    let _env = thegn_core::testenv::EnvGuard::mutate_pairs(&[
        ("GITLAB_TOKEN", Some("transport-ambient-gitlab")),
        ("GH_TOKEN", Some("transport-ambient-gh")),
    ]);
    let gitlab_remote =
        parse_gitlab_remote(&format!("https://{HOST_A}/g/r")).expect("valid fixture remote");
    let github_remote =
        parse_github_remote("https://github.example.test/o/r").expect("valid fixture remote");
    for (name, loc) in non_local_locations() {
        assert!(loc.is_remote(), "{name}");
        for credential in [
            GitlabCredential::CliManaged,
            GitlabCredential::Configured {
                token: TOKEN_A.into(),
                explicit: false,
            },
        ] {
            let gitlab = GitlabCi {
                credential,
                ..GitlabCi::default()
            };
            let cmd = gitlab
                .command(&loc, &gitlab_remote, &["api", "projects/g%2Fr/pipelines"])
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let keys = env_keys(&cmd);
            assert!(!keys.iter().any(|key| key == "GITLAB_TOKEN"), "{name}");
            assert!(!keys.iter().any(|key| key == "GH_TOKEN"), "{name}");
            assert!(keys.iter().any(|key| key == "GITLAB_HOST"), "{name}");
            let argv = argv_text(&cmd);
            assert!(
                argv.contains(&format!("--hostname={HOST_A}")),
                "{name}: {argv}"
            );
            assert!(!argv.contains(TOKEN_A), "{name}: {argv}");
        }
        let github = GithubCi {
            remote: Some(github_remote.clone()),
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: None,
        };
        let cmd = github
            .command(&loc, &["run", "list"])
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let keys = env_keys(&cmd);
        assert!(!keys.iter().any(|key| key == "GH_TOKEN"), "{name}");
        assert!(!keys.iter().any(|key| key == "GITHUB_TOKEN"), "{name}");
        assert!(!argv_text(&cmd).contains("transport-ambient-gh"), "{name}");
    }
}

#[test]
fn transport_explicit_token_is_refused_off_host_without_echoing_it() {
    let gitlab_remote =
        parse_gitlab_remote(&format!("https://{HOST_A}/g/r")).expect("valid fixture remote");
    let gitlab = GitlabCi {
        credential: GitlabCredential::Configured {
            token: TOKEN_A.into(),
            explicit: true,
        },
        ..GitlabCi::default()
    };
    for (name, loc) in non_local_locations() {
        let error = gitlab
            .command(&loc, &gitlab_remote, &["api", "projects/g%2Fr/pipelines"])
            .map(|_| ())
            .expect_err(name);
        let rendered = error.to_string();
        assert!(
            rendered.contains("cannot be forwarded"),
            "{name}: {rendered}"
        );
        assert!(!rendered.contains(TOKEN_A), "{name}: {rendered}");
    }
}

#[test]
fn transport_two_hosts_and_accounts_never_cross_in_either_order() {
    let Some(fake) = FakeGlab::new("printf '[]\\n'\n") else {
        return;
    };
    let _env = fake.path_guard(&[("GITLAB_TOKEN", None)]);
    let (_repo_a, loc_a) = repo_with_origin(&format!("https://{HOST_A}/group/repo.git"));
    let (_repo_b, loc_b) = repo_with_origin(&format!("https://{HOST_B}/group/repo.git"));
    let (client_a, _) = provider_and_context_for(&loc_a, &gitlab_cfg(TOKEN_A)).unwrap();
    let (client_b, _) = provider_and_context_for(&loc_b, &gitlab_cfg(TOKEN_B)).unwrap();

    for a_first in [true, false] {
        fake.reset_log();
        if a_first {
            client_a.runs(&loc_a, None, 10).unwrap();
            client_b.runs(&loc_b, None, 10).unwrap();
        } else {
            client_b.runs(&loc_b, None, 10).unwrap();
            client_a.runs(&loc_a, None, 10).unwrap();
        }
        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "a_first={a_first}: {calls:?}");
        for (host, marker, token, argv) in &calls {
            assert_eq!(marker, "set", "a_first={a_first}");
            match host.as_str() {
                HOST_A => assert_eq!(token, TOKEN_A, "a_first={a_first}"),
                HOST_B => assert_eq!(token, TOKEN_B, "a_first={a_first}"),
                other => panic!("unexpected host {other}"),
            }
            assert!(!argv.contains(TOKEN_A) && !argv.contains(TOKEN_B), "{argv}");
        }
        let hosts: Vec<&str> = calls.iter().map(|call| call.0.as_str()).collect();
        let expected = if a_first {
            vec![HOST_A, HOST_B]
        } else {
            vec![HOST_B, HOST_A]
        };
        assert_eq!(hosts, expected);
    }
}

#[test]
fn transport_cli_managed_mode_exports_no_token_env_and_still_calls() {
    let Some(fake) = FakeGlab::new("printf '[]\\n'\n") else {
        return;
    };
    // The shipped default `env:GITLAB_TOKEN` with the variable unset is the
    // CLI-managed (`glab auth login`) mode.
    let _env = fake.path_guard(&[("GITLAB_TOKEN", None), ("GH_TOKEN", None)]);
    let (_repo, loc) = repo_with_origin(&format!("https://{HOST_A}/group/repo.git"));
    let (client, _) = provider_and_context_for(
        &loc,
        &CiConfig {
            provider: CiProviderKind::Gitlab,
            ..CiConfig::default()
        },
    )
    .unwrap();
    client.runs(&loc, None, 10).unwrap();
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let (host, marker, token, _argv) = &calls[0];
    assert_eq!(host, HOST_A);
    assert_eq!(marker, "", "no token variable may be exported");
    assert_eq!(token, "");

    let gitlab = GitlabCi {
        credential: GitlabCredential::CliManaged,
        ..GitlabCi::default()
    };
    let remote = parse_gitlab_remote(&format!("https://{HOST_A}/group/repo")).unwrap();
    let cmd = gitlab.command(&loc, &remote, &["api", "x"]).unwrap();
    assert!(!env_keys(&cmd).iter().any(|key| key == "GITLAB_TOKEN"));
}

#[test]
fn transport_failing_cli_error_text_never_contains_the_token() {
    let Some(fake) =
        FakeGlab::new("printf '401 Unauthorized for %s\\n' \"$GITLAB_TOKEN\" >&2\nexit 1\n")
    else {
        return;
    };
    let _env = fake.path_guard(&[("GITLAB_TOKEN", None)]);
    let (_repo, loc) = repo_with_origin(&format!("https://{HOST_A}/group/repo.git"));
    let (client, _) = provider_and_context_for(&loc, &gitlab_cfg(TOKEN_A)).unwrap();
    let error = client.runs(&loc, None, 10).expect_err("fake glab exits 1");
    let rendered = error.to_string();
    let debug = format!("{error:?}");
    assert!(!rendered.contains(TOKEN_A), "{rendered}");
    assert!(!debug.contains(TOKEN_A), "{debug}");
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "the fake must have run: {calls:?}");
    assert_eq!(calls[0].2, TOKEN_A, "the token reached the child env only");
}

/// A configured token with no classifier keyword in it, so a stderr that
/// echoes it stays unclassified and reaches `CiError::Other`.
const PLAIN_TOKEN: &str = "Qz7mWv3kPx9LrT2nHs4c";

fn exit_error(stderr: &str) -> crate::issue::gh_run::RunError {
    crate::issue::gh_run::RunError::Exit {
        code: Some(1),
        stderr: stderr.into(),
    }
}

#[test]
fn transport_unclassified_failure_redacts_the_configured_token() {
    let Some(fake) = FakeGlab::new(
        "printf 'glab: unexpected failure for %s\\n' \"$GITLAB_TOKEN\" >&2\nexit 1\n",
    ) else {
        return;
    };
    let _env = fake.path_guard(&[("GITLAB_TOKEN", None)]);
    let (_repo, loc) = repo_with_origin(&format!("https://{HOST_A}/group/repo.git"));
    let (client, _) = provider_and_context_for(&loc, &gitlab_cfg(PLAIN_TOKEN)).unwrap();
    let error = client.runs(&loc, None, 10).expect_err("fake glab exits 1");
    assert!(matches!(error, CiError::Other(_)), "{error:?}");
    let rendered = error.to_string();
    assert!(!rendered.contains(PLAIN_TOKEN), "{rendered}");
    assert!(
        rendered.contains(thegn_core::ci_log::REDACTION_MARKER),
        "{rendered}"
    );
    assert!(rendered.contains("unexpected failure"), "{rendered}");
    let calls = fake.calls();
    assert!(!calls.is_empty(), "the fake must have run");
    assert!(
        calls.iter().all(|call| call.2 == PLAIN_TOKEN),
        "the token reached the child env only"
    );
}

#[test]
fn transport_provider_token_shapes_are_scrubbed_without_a_configured_token() {
    let glpat = format!("glpat-{}", "Ab1".repeat(8));
    let ghp = format!("ghp_{}", "Cd2".repeat(8));
    let error = ci_run_error(
        exit_error(&format!(
            "glab: bad response {glpat} then gh: {ghp} rejected"
        )),
        &[],
    );
    assert!(matches!(error, CiError::Other(_)), "{error:?}");
    let rendered = error.to_string();
    assert!(!rendered.contains(&glpat), "{rendered}");
    assert!(!rendered.contains(&ghp), "{rendered}");
    assert!(
        rendered.contains(thegn_core::ci_log::REDACTION_MARKER),
        "{rendered}"
    );
    assert!(rendered.contains("bad response"), "{rendered}");

    let header = format!("Authorization: Bearer {PLAIN_TOKEN}\nPRIVATE-TOKEN: {PLAIN_TOKEN}");
    let scrubbed = scrub_cli_text(&header, &[]);
    assert!(!scrubbed.contains(PLAIN_TOKEN), "{scrubbed}");
}

#[test]
fn transport_short_configured_value_does_not_mangle_error_text() {
    let stderr = "abc failed at abcdef";
    let error = ci_run_error(exit_error(stderr), &["abc".into()]);
    assert_eq!(error.to_string(), stderr);
}

#[test]
fn transport_redaction_keeps_401_classification() {
    let stderr = format!("HTTP 401 for {PLAIN_TOKEN}");
    let error = ci_run_error(exit_error(&stderr), &[PLAIN_TOKEN.into()]);
    assert_eq!(error, classify_stderr(&stderr));
    assert_eq!(error, CiError::NotAuthenticated);
    assert!(!error.to_string().contains(PLAIN_TOKEN));
}

#[test]
fn transport_command_secrets_include_the_exported_configured_token() {
    let _env = thegn_core::testenv::EnvGuard::mutate_pairs(&[("GITLAB_TOKEN", None)]);
    let repo = tempfile::tempdir().unwrap();
    let loc = GitLoc::Local(repo.path().to_path_buf());
    let remote = parse_gitlab_remote(&format!("https://{HOST_A}/g/r")).unwrap();
    let gitlab = GitlabCi {
        credential: GitlabCredential::Configured {
            token: PLAIN_TOKEN.into(),
            explicit: true,
        },
        ..GitlabCi::default()
    };
    let cmd = gitlab.command(&loc, &remote, &["api", "x"]).unwrap();
    assert!(
        command_secrets(&cmd)
            .iter()
            .any(|secret| secret == PLAIN_TOKEN)
    );
}

#[test]
fn transport_json_decode_errors_do_not_quote_token_values() {
    let glpat = format!("glpat-{}", "Ab1".repeat(8));
    let json = format!("[\"{glpat}\"]");
    let error = decode_provider_json::<Vec<u64>>(&json, "provider runs").unwrap_err();
    assert!(matches!(error, CiError::Malformed(_)), "{error:?}");
    let rendered = error.to_string();
    assert!(!rendered.contains(&glpat), "{rendered}");
    assert!(
        rendered.contains(thegn_core::ci_log::REDACTION_MARKER),
        "{rendered}"
    );
}
