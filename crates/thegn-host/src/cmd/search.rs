//! `thegn search` — workspace-wide search & replace from the CLI (THE-5).
//!
//! The headless projection of the `search.query` / `search.replace` catalog
//! rows. Query mode prints matches (JSON with `--json`); `--replace <tpl>`
//! prints a plan (dry run, scooter's `--no-tui` analogue); adding `--apply`
//! performs the replacement through the SAME guarded write path
//! ([`crate::search_apply::apply`]) the overlay uses. `--structural` routes
//! through the ast-grep seam (rewrites still land via the guarded path).

use anyhow::{Context, bail};
use thegn_core::outln;
use thegn_core::search_replace::{
    Edit, SearchMode, SearchSpec, SpanEdit, StructuralSpec, WalkFilter, render_after_line,
};

use crate::search_apply::{self, FileEdits};

/// `thegn search <pattern> [flags]`.
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// The search pattern (literal by default; a regex with `--regex`; an
    /// ast-grep AST pattern with `--structural`).
    pub pattern: String,
    /// Interpret the pattern as a regex (capture groups expand in `--replace`).
    #[arg(long)]
    pub regex: bool,
    /// Case-sensitive match (default: case-insensitive).
    #[arg(long, short = 's')]
    pub case: bool,
    /// Whole-word match only.
    #[arg(long, short = 'w')]
    pub word: bool,
    /// Include-glob (repeatable). If any are given, only matching paths search.
    #[arg(long = "glob", short = 'g')]
    pub glob: Vec<String>,
    /// Exclude-glob (repeatable). Always wins over includes.
    #[arg(long = "exclude", short = 'x')]
    pub exclude: Vec<String>,
    /// Descend hidden files/dirs (dotfiles).
    #[arg(long)]
    pub hidden: bool,
    /// Do not honor `.gitignore` (`.git/` is still excluded).
    #[arg(long = "no-ignore")]
    pub no_ignore: bool,
    /// Structural (AST) search via the ast-grep seam.
    #[arg(long = "structural", visible_alias = "sg")]
    pub structural: bool,
    /// Language for structural search (`rust`, `ts`, …). Empty ⇒ inferred.
    #[arg(long, default_value = "")]
    pub lang: String,
    /// Replacement template. Prints a plan (dry run) unless `--apply` is given.
    #[arg(long)]
    pub replace: Option<String>,
    /// Actually write the replacement (through the guarded path). Requires
    /// `--replace`.
    #[arg(long)]
    pub apply: bool,
    /// Machine-readable JSON output (query mode).
    #[arg(long)]
    pub json: bool,
    /// Cap on results / matches (default: `[search] max_results`).
    #[arg(long)]
    pub max: Option<usize>,
    /// The worktree to search (default: `$THEGN_WORKTREE` / the cwd's git root).
    #[arg(long)]
    pub path: Option<String>,
}

pub fn run(cfg: &thegn_core::config::Config, args: Args) -> anyhow::Result<()> {
    let root = super::resolve_worktree(args.path.clone());
    let max = args.max.unwrap_or(cfg.search.max_results);
    let filter = WalkFilter {
        include_globs: args.glob.clone(),
        exclude_globs: args.exclude.clone(),
        respect_gitignore: !args.no_ignore && cfg.search.respect_gitignore,
        include_hidden: args.hidden || cfg.search.include_hidden,
    };

    if args.structural {
        run_structural(cfg, &root, &args, max)
    } else {
        run_textual(&root, &args, &filter, max)
    }
}

// ── Textual tier ────────────────────────────────────────────────────────────

fn run_textual(
    root: &std::path::Path,
    args: &Args,
    filter: &WalkFilter,
    max: usize,
) -> anyhow::Result<()> {
    let mode = if args.regex {
        SearchMode::Regex
    } else {
        SearchMode::Literal
    };
    let spec = SearchSpec {
        query: args.pattern.clone(),
        mode,
        case_sensitive: args.case,
        whole_word: args.word,
    };
    let (matches, truncated) = crate::search_worker::search_collect(root, &spec, filter, max)
        .map_err(|e| anyhow::anyhow!(e))?;

    let Some(tpl) = &args.replace else {
        // Query mode.
        print_query(&matches, truncated, args.json);
        return Ok(());
    };

    // Replace mode: build line edits grouped by file.
    let mut by_file: std::collections::BTreeMap<String, Vec<Edit>> =
        std::collections::BTreeMap::new();
    for m in &matches {
        by_file
            .entry(m.path.clone())
            .or_default()
            .push(Edit::from_match(m, tpl, mode));
    }

    if !args.apply {
        // Dry run: print the plan with before/after previews.
        print_plan_textual(&matches, tpl, mode, truncated);
        return Ok(());
    }

    let files: Vec<(String, FileEdits)> = by_file
        .into_iter()
        .map(|(p, e)| (p, FileEdits::Line(e)))
        .collect();
    let report = search_apply::apply(root, files);
    outln!("{}", report.summary_line());
    for f in &report.files {
        if let Some(err) = &f.error {
            outln!("  ! {}: {err}", f.path);
        }
    }
    Ok(())
}

// ── Structural tier ─────────────────────────────────────────────────────────

fn run_structural(
    cfg: &thegn_core::config::Config,
    root: &std::path::Path,
    args: &Args,
    max: usize,
) -> anyhow::Result<()> {
    let Some(provider) = crate::structural::provider(cfg.search.structural) else {
        bail!(
            "structural search is disabled or unavailable (`[search] structural = \"{}\"`)",
            cfg.search.structural.as_str()
        );
    };
    let spec = StructuralSpec {
        pattern: args.pattern.clone(),
        lang: args.lang.clone(),
        rewrite: args.replace.clone(),
    };

    let matches = if args.replace.is_some() {
        provider
            .rewrite(root, &spec)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    } else {
        provider
            .search(root, &spec)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    };
    let matches: Vec<_> = matches
        .into_iter()
        .take(if max == 0 { usize::MAX } else { max })
        .collect();

    let Some(_tpl) = &args.replace else {
        // Query mode.
        if args.json {
            super::emit_json(&matches)?;
        } else {
            for m in &matches {
                outln!("{}:{}: {}", m.path, m.line, m.text.replace('\n', "⏎"));
            }
            outln!("{} match(es)", matches.len());
        }
        return Ok(());
    };

    // Replace: fold ast-grep's computed replacements into SpanEdits via the file
    // content (snapshot for drift), grouped by file.
    let mut by_file: std::collections::BTreeMap<String, Vec<SpanEdit>> =
        std::collections::BTreeMap::new();
    for m in &matches {
        let abs = root.join(&m.path);
        let content = std::fs::read_to_string(&abs)
            .with_context(|| format!("re-reading {} for structural rewrite", m.path))?;
        if let Some(se) = SpanEdit::from_structural(&content, m) {
            by_file.entry(m.path.clone()).or_default().push(se);
        }
    }

    if !args.apply {
        for m in &matches {
            if let Some(rep) = &m.replacement {
                outln!(
                    "{}:{}: {} → {}",
                    m.path,
                    m.line,
                    m.text.replace('\n', "⏎"),
                    rep.replace('\n', "⏎")
                );
            }
        }
        outln!(
            "{} rewrite(s) planned (dry run — pass --apply to write)",
            matches.iter().filter(|m| m.replacement.is_some()).count()
        );
        return Ok(());
    }

    let files: Vec<(String, FileEdits)> = by_file
        .into_iter()
        .map(|(p, e)| (p, FileEdits::Span(e)))
        .collect();
    let report = search_apply::apply(root, files);
    outln!("{}", report.summary_line());
    for f in &report.files {
        if let Some(err) = &f.error {
            outln!("  ! {}: {err}", f.path);
        }
    }
    Ok(())
}

// ── Printing ────────────────────────────────────────────────────────────────

fn print_query(matches: &[thegn_core::search_replace::Match], truncated: bool, json: bool) {
    if json {
        #[derive(serde::Serialize)]
        struct Row<'a> {
            path: &'a str,
            line: usize,
            col: usize,
            text: &'a str,
        }
        let rows: Vec<Row> = matches
            .iter()
            .map(|m| Row {
                path: &m.path,
                line: m.line,
                col: m.byte_start,
                text: &m.line_text,
            })
            .collect();
        let _ = super::emit_json(&serde_json::json!({
            "matches": rows,
            "truncated": truncated,
        }));
        return;
    }
    for m in matches {
        outln!("{}:{}: {}", m.path, m.line, m.line_text.trim_end());
    }
    outln!(
        "{} match(es){}",
        matches.len(),
        if truncated { " (truncated)" } else { "" }
    );
}

fn print_plan_textual(
    matches: &[thegn_core::search_replace::Match],
    tpl: &str,
    mode: SearchMode,
    truncated: bool,
) {
    let mut files = std::collections::BTreeSet::new();
    for m in matches {
        files.insert(m.path.clone());
        outln!(
            "{}:{}\n  - {}\n  + {}",
            m.path,
            m.line,
            m.line_text.trim_end(),
            render_after_line(m, tpl, mode).trim_end()
        );
    }
    outln!(
        "plan: {} replacement(s) across {} file(s){} — dry run, pass --apply to write",
        matches.len(),
        files.len(),
        if truncated { " (truncated)" } else { "" }
    );
}
