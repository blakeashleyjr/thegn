use super::command::*;
use super::*;
use crate::config::{GitCmdOutput, GitCommand};
use crate::remote::{GitLoc, SshTarget};

fn config(command: &str) -> GitCommand {
    GitCommand {
        key: "x".into(),
        context: "global".into(),
        command: command.into(),
        argv: vec![],
        template_policy: Some("safe".into()),
        description: None,
        output: GitCmdOutput::Popup,
        prompts: vec![],
    }
}

fn context(value: &str) -> TemplateCtx {
    TemplateCtx {
        selected_commit: Some(CommitVars {
            sha: value.into(),
            short: value.into(),
            subject: value.into(),
            author: value.into(),
        }),
        selected_branch: Some(BranchVars {
            name: value.into(),
            upstream: Some(value.into()),
        }),
        checked_out_branch: Some(BranchVars {
            name: value.into(),
            upstream: Some(value.into()),
        }),
        selected_file: Some(value.into()),
        selected_stash: Some(StashVars {
            index: 2,
            message: value.into(),
        }),
        worktree_path: Some(value.into()),
        prompt_responses: BTreeMap::from([("Answer".into(), value.into())]),
    }
}

const PATHS: &[&str] = &[
    "SelectedCommit.Sha",
    "SelectedCommit.Short",
    "SelectedCommit.Subject",
    "SelectedCommit.Author",
    "SelectedLocalCommit.Subject",
    "SelectedBranch.Name",
    "SelectedBranch.Upstream",
    "SelectedLocalBranch.Name",
    "CheckedOutBranch.Name",
    "CheckedOutBranch.Upstream",
    "SelectedFile",
    "SelectedFile.Name",
    "SelectedStash.Message",
    "SelectedStashEntry.Message",
    "WorktreePath",
    "Form.Answer",
    "Prompts.Answer",
];

#[test]
fn every_data_family_is_inert_across_modes_and_transport_projections() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("must-not-exist");
    let payload = format!(
        "spaces ' \" ; touch {} ; $(touch {}) `touch {}`\n > {}",
        sentinel.display(),
        sentinel.display(),
        sentinel.display(),
        sentinel.display()
    );
    let ctx = context(&payload);
    let path = dir.path().to_str().unwrap();
    let locations = [
        GitLoc::Local(dir.path().into()),
        GitLoc::Remote {
            ssh: SshTarget::plain("fixture.invalid".into(), 22, false),
            path: path.into(),
        },
        GitLoc::Provider {
            control_prefix: vec!["fixture-provider".into()],
            path: path.into(),
        },
    ];
    for name in PATHS {
        for output in [
            GitCmdOutput::Popup,
            GitCmdOutput::None,
            GitCmdOutput::Terminal,
        ] {
            for argv in [false, true] {
                let mut cmd = config(&format!("printf '%s' {{{{.{name}}}}}"));
                cmd.output = output;
                if argv {
                    cmd.command.clear();
                    cmd.template_policy = None;
                    cmd.argv = vec!["printf".into(), "%s".into(), format!("{{{{.{name}}}}}")];
                }
                let expanded = compile(&cmd, &ctx).unwrap();
                assert!(!format!("{expanded:?}").contains(&payload));
                for loc in &locations {
                    let mut process = expanded.command(loc);
                    let result = if output == GitCmdOutput::Terminal {
                        let argv = expanded.terminal_argv();
                        std::process::Command::new(&argv[0])
                            .args(&argv[1..])
                            .output()
                            .unwrap()
                    } else if matches!(loc, GitLoc::Local(_)) {
                        process.output().unwrap()
                    } else {
                        // Execute the exact remote payload locally; never start SSH/provider.
                        let script = if output == GitCmdOutput::Terminal {
                            expanded.shell_script()
                        } else {
                            process.get_args().last().unwrap().to_str().unwrap().into()
                        };
                        std::process::Command::new("sh")
                            .args(["-c", &script])
                            .output()
                            .unwrap()
                    };
                    assert!(result.status.success(), "{name} {output:?}");
                    assert_eq!(result.stdout, payload.as_bytes(), "{name} {output:?}");
                    assert!(!sentinel.exists());
                }
            }
        }
    }
}

#[test]
fn shell_contexts_are_conservatively_refused() {
    for script in [
        "echo '{{.SelectedFile}}'",
        "echo \"{{.SelectedFile}}\"",
        "echo >{{.SelectedFile}}",
        "echo > {{.SelectedFile}}",
        "X={{.SelectedFile}} echo",
        "echo x{{.SelectedFile}}",
        "echo {{.SelectedFile}}x",
        "echo {{.SelectedFile}};",
        "f() { echo {{.SelectedFile}}; }",
        "cat <<EOF\n{{.SelectedFile}}\nEOF",
        "echo # {{.SelectedFile}}",
        "echo $(printf {{.SelectedFile}})",
        "echo `printf {{.SelectedFile}}`",
        "echo $(({{.SelectedFile}}))",
        "echo \\\n{{.SelectedFile}}",
        "{{.SelectedFile}}",
        "printf x ; {{.SelectedFile}}",
        "printf x\n{{.SelectedFile}}",
        "if {{.SelectedFile}} ; then true ; fi",
    ] {
        assert!(
            compile(&config(script), &context("unsafe")).is_err(),
            "{script}"
        );
    }
    assert!(
        compile(
            &config("printf '%s' {{.SelectedFile}} ; printf done"),
            &context("safe")
        )
        .is_ok()
    );
}

#[test]
fn legacy_migration_and_errors_do_not_echo_values() {
    let mut cmd = config("printf '%s' {{.SelectedFile}}");
    cmd.template_policy = None;
    let errors = validate_commands(&[cmd.clone()]);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].starts_with("git_commands[0]:"));
    assert!(compile(&cmd, &context("secret")).is_err());
    cmd.command = "printf '%s' \"$HOME\"".into();
    assert_eq!(
        compile(&cmd, &context("secret")).unwrap().shell_script(),
        cmd.command
    );
    cmd = config("printf '%s' {{.Form.SENSITIVE_UNKNOWN_KEY}}");
    let error = compile(&cmd, &TemplateCtx::default()).unwrap_err();
    assert!(!format!("{error} {error:?}").contains("SENSITIVE_UNKNOWN_KEY"));
    cmd.command = "printf '%s' {{.SelectedFile | SENSITIVE_FILTER}}".into();
    assert!(!validate_commands(&[cmd])[0].contains("SENSITIVE_FILTER"));
}

#[test]
fn raw_requires_both_explicit_policy_and_filter() {
    let mut cmd = config("printf '%s' {{.SelectedFile | dangerously_unquoted_shell}}");
    assert!(compile(&cmd, &context("raw")).is_err());
    cmd.template_policy = Some("dangerously_allow_raw".into());
    let raw = compile(&cmd, &context("raw ; printf INJECTION")).unwrap();
    assert!(raw.shell_script().contains("raw ; printf INJECTION"));
    cmd.command = "printf '%s' {{.SelectedFile}}".into();
    assert!(
        compile(&cmd, &context("safe ; printf nope"))
            .unwrap()
            .shell_script()
            .contains("'safe ; printf nope'")
    );
    cmd.command.clear();
    cmd.argv = vec![
        "printf".into(),
        "{{.SelectedFile | dangerously_unquoted_shell}}".into(),
    ];
    cmd.template_policy = None;
    assert!(compile(&cmd, &context("raw")).is_err());
}

#[test]
fn invalid_and_oversized_values_fail_without_partial_program() {
    for value in ["contains\0nul".into(), "x".repeat(1024 * 1024 + 1)] {
        assert!(compile(&config("printf '%s' {{.SelectedFile}}"), &context(&value)).is_err());
    }
    for script in [
        "echo {{",
        "echo {{ }}",
        "echo {{.Unknown}}",
        "echo {{.SelectedFile | unknown}}",
    ] {
        assert!(compile(&config(script), &context("secret")).is_err());
    }
}

#[test]
fn strict_validation_rejects_only_the_command_not_config_deserialization() {
    let body = "[sandbox]\ndefault = 'none'\n[[git_commands]]\nkey='x'\ncommand='echo {{.SelectedFile}}'\n";
    let cfg: crate::config::Config = toml::from_str(body).unwrap();
    assert_eq!(cfg.git_commands.len(), 1);
    assert!(
        crate::config_validate::validate_str(body)
            .iter()
            .any(|e| e.contains("git_commands[0]") && e.contains("legacy"))
    );
}
