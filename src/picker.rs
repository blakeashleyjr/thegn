//! Interactive pickers. Prefers gum/fzf; falls back to a numbered stdin prompt.

use crate::config::Config;
use crate::repo;
use crate::util;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

/// Present `options` under `prompt`; returns the chosen label, or None if the
/// user aborts. `pref` is the configured picker ("auto"|"gum"|"fzf"|"select").
pub fn pick(prompt: &str, options: &[String], pref: &str) -> Option<String> {
    if options.is_empty() {
        return None;
    }
    let backend = if pref == "auto" {
        if util::have("gum") {
            "gum"
        } else if util::have("fzf") {
            "fzf"
        } else {
            "select"
        }
    } else {
        pref
    };

    match backend {
        "gum" => external(prompt, options, "gum", &["choose", "--header", prompt]),
        "fzf" => external(
            prompt,
            options,
            "fzf",
            &[
                "--reverse",
                "--height=~40%",
                "--no-multi",
                &format!("--prompt={prompt} > "),
            ],
        ),
        _ => select_fallback(prompt, options),
    }
}

fn external(_prompt: &str, options: &[String], bin: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    {
        let stdin = child.stdin.as_mut()?;
        let _ = stdin.write_all(options.join("\n").as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let choice = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if choice.is_empty() {
        None
    } else {
        Some(choice)
    }
}

fn select_fallback(prompt: &str, options: &[String]) -> Option<String> {
    let stderr = std::io::stderr();
    let mut e = stderr.lock();
    for (i, o) in options.iter().enumerate() {
        let _ = writeln!(e, "  {}) {o}", i + 1);
    }
    let _ = write!(e, "{prompt} ");
    let _ = e.flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    let idx: usize = line.trim().parse().ok()?;
    options.get(idx.checked_sub(1)?).cloned()
}

/// Prompt for a repo: pick one discovered under repo_roots, or clone a URL.
/// Returns a repo path or a clone URL.
pub fn pick_repo(cfg: &Config) -> Option<String> {
    let mut options = repo::discover_repos(cfg);
    let clone_label = "+ clone a URL…".to_string();
    options.push(clone_label.clone());

    let choice = pick("Select a repo", &options, &cfg.picker)?;
    if choice == clone_label {
        let url = if util::have("gum") {
            let out = Command::new("gum")
                .args(["input", "--placeholder", "git@github.com:org/repo.git"])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            eprint!("Clone URL: ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).ok()?;
            line.trim().to_string()
        };
        if url.is_empty() { None } else { Some(url) }
    } else {
        Some(choice)
    }
}
