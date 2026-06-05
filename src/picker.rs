//! Interactive pickers. Prefers gum/fzf; falls back to a numbered stdin prompt.

use crate::config::Config;
use crate::repo;
use crate::util;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// The configured accent as "#rrggbb", set once at startup so every picker
/// (gum/fzf) can theme without threading cfg through each call site.
static ACCENT: OnceLock<String> = OnceLock::new();

pub fn set_accent(hex: &str) {
    let _ = ACCENT.set(hex.to_string());
}

fn accent_hex() -> &'static str {
    ACCENT.get().map(String::as_str).unwrap_or("#76eede")
}

/// fzf `--color` spec themed to the palette (accent for selection/prompt,
/// dim/ghost for header/info, border in the panel border color).
pub fn fzf_color() -> String {
    let a = accent_hex();
    format!(
        "--color=fg+:#e0e4f0,bg+:-1,hl:{a},hl+:{a},prompt:{a},pointer:{a},\
marker:{a},spinner:{a},header:#65687f,info:#65687f,border:#3e445c,gutter:-1"
    )
}

/// gum environment for themed `choose`/`input` (ignored by older gum, so it
/// degrades gracefully). The accent drives the cursor/match/prompt.
fn gum_env(cmd: &mut Command) {
    let a = accent_hex();
    cmd.env("GUM_CHOOSE_CURSOR_FOREGROUND", a)
        .env("GUM_CHOOSE_SELECTED_FOREGROUND", a)
        .env("GUM_CHOOSE_HEADER_FOREGROUND", "#9aa0b4")
        .env("GUM_CHOOSE_ITEM_FOREGROUND", "#e0e4f0")
        .env("GUM_INPUT_CURSOR_FOREGROUND", a)
        .env("GUM_INPUT_PROMPT_FOREGROUND", a)
        .env("GUM_FILTER_INDICATOR_FOREGROUND", a);
}

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
                "--pointer=▌",
                &fzf_color(),
                &format!("--prompt={prompt} ❯ "),
            ],
        ),
        _ => select_fallback(prompt, options),
    }
}

fn external(_prompt: &str, options: &[String], bin: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    if bin == "gum" {
        gum_env(&mut cmd);
    }
    let mut child = cmd
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

/// Free-text prompt (gum input if present, else a stderr prompt). None if empty.
pub fn prompt(label: &str) -> Option<String> {
    let s = if util::have("gum") {
        let out = Command::new("gum")
            .args(["input", "--placeholder", label])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        eprint!("{label}: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).ok()?;
        line.trim().to_string()
    };
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The sidebar's "+ new workspace" picker: a slick `fzf` over every git repo
/// under `$HOME`, with a live git preview (graph log + short status). Returns an
/// absolute repo path, or None on cancel. Falls back to the classic repo picker
/// when fzf is missing or nothing is found under home.
pub fn pick_dir_home(cfg: &Config) -> Option<String> {
    let home = util::home();
    let repos = repo::discover_repos_in(&home, 6);
    if repos.is_empty() || !util::have("fzf") {
        return pick_repo(cfg);
    }

    // Absolute paths kept verbatim (no tilde) so fzf's {} substitution feeds
    // `git -C {}` directly. fzf runs the preview via `$SHELL -c`, which may be
    // fish, bash, sh, … — so the command is kept to constructs that parse the
    // same in all of them: no `if/then/fi` (fish chokes on it), just `;`-joined
    // commands and `2>/dev/null` redirections. Every entry is already a git repo
    // (discover_repos_in only yields dirs with a .git), so no non-repo branch is
    // needed. The wrapped binary's PATH has git but not sed/find, so we use
    // neither.
    let preview = "git -C {} -c color.ui=always log --oneline --graph --decorate -20 2>/dev/null; \
echo; \
git -C {} -c color.ui=always -c status.relativePaths=false status -s 2>/dev/null";
    let args: &[&str] = &[
        "--reverse",
        "--ansi",
        "--height=100%",
        "--border=rounded",
        "--margin=1,2",
        "--prompt=new workspace ❯ ",
        "--pointer=▌",
        "--header=↵ open a repo under ~ as a workspace   ·   esc to cancel",
        "--preview",
        preview,
        "--preview-window=right,55%,border-left",
        &fzf_color(),
    ];
    external("new workspace", &repos, "fzf", args).map(|s| util::expand_tilde(&s))
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
        if url.is_empty() {
            None
        } else {
            Some(url)
        }
    } else {
        Some(choice)
    }
}
