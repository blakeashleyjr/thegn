//! `thegn skills` — inspect and seed the embedded/configured skill registry.

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use thegn_core::config::Config;
use thegn_core::skills::{SeedPhase, SkillSource, validate_name};

#[derive(Subcommand, Clone)]
pub enum Action {
    /// List skill metadata in deterministic name order.
    List {
        /// Emit one JSON object with `skills` and `diagnostics`.
        #[arg(long)]
        json: bool,
    },
    /// Print one canonical skill document without writing anything.
    Show {
        /// Skill package name.
        name: String,
        /// Emit metadata and document as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Seed all configured harnesses into one worktree.
    Seed {
        /// Target worktree (default: current worktree resolution).
        #[arg(long)]
        worktree: Option<String>,
        /// Emit one JSON report.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Serialize)]
struct ShowOutput {
    skill: thegn_svc::control::SkillInfo,
    document: String,
    diagnostics: Vec<String>,
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(cfg, json),
        Action::Show { name, json } => show(cfg, &name, json),
        Action::Seed { worktree, json } => seed(cfg, worktree, json),
    }
}

fn list(cfg: &Config, json: bool) -> Result<()> {
    let loaded = crate::skill_seed::load_registry(cfg);
    let output = thegn_svc::control::SkillsList {
        skills: loaded
            .registry
            .iter()
            .map(|(_, skill)| crate::skill_seed::metadata(skill))
            .collect(),
        diagnostics: loaded.diagnostics,
    };
    if json {
        return super::emit_json(&output);
    }
    thegn_core::outln!(
        "{:<18} {:<12} {:<20} {:<24} {:<9} DESCRIPTION",
        "NAME",
        "GATE",
        "HARNESSES",
        "WHEN",
        "SOURCE"
    );
    for skill in &output.skills {
        let source = match skill.source {
            SkillSource::Embedded { .. } => "embedded",
            SkillSource::User { .. } => "user",
        };
        thegn_core::outln!(
            "{:<18} {:<12} {:<20} {:<24} {:<9} {}",
            skill.name,
            skill.gate,
            skill.harnesses.join(","),
            skill.when.join(","),
            source,
            skill.description
        );
    }
    print_diagnostics(&output.diagnostics);
    Ok(())
}

fn show(cfg: &Config, name: &str, json: bool) -> Result<()> {
    validate_name(name).map_err(|error| anyhow::anyhow!(error))?;
    let loaded = crate::skill_seed::load_registry(cfg);
    let skill = loaded
        .registry
        .get(name)
        .ok_or_else(|| anyhow::Error::new(super::NotFound(format!("skill not found: {name}"))))?;
    let output = ShowOutput {
        skill: crate::skill_seed::metadata(skill),
        document: crate::skill_seed::shown_document(skill),
        diagnostics: loaded.diagnostics,
    };
    if json {
        return super::emit_json(&output);
    }
    thegn_core::out!("{}", output.document);
    print_diagnostics(&output.diagnostics);
    Ok(())
}

fn seed(cfg: &Config, worktree: Option<String>, json: bool) -> Result<()> {
    let worktree = super::resolve_worktree(worktree);
    let report = crate::skill_seed::seed(cfg, &worktree, SeedPhase::Explicit)?;
    if json {
        return super::emit_json(&report);
    }
    for row in &report.files {
        thegn_core::outln!("{:<8} {:<22} {}", row.status, row.harness, row.path);
    }
    if report.files.is_empty() {
        thegn_core::outln!("no skill files changed");
    }
    print_diagnostics(&report.diagnostics);
    Ok(())
}

fn print_diagnostics(diagnostics: &[String]) {
    for diagnostic in diagnostics {
        thegn_core::msg::warn(&format!("skills: {diagnostic}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn action_grammar_is_stable() {
        use clap::CommandFactory as _;
        let mut command = crate::Cli::command();
        command.build();
        let skills = command.find_subcommand("skills").unwrap();
        assert!(skills.find_subcommand("list").is_some());
        assert!(skills.find_subcommand("show").is_some());
        assert!(skills.find_subcommand("seed").is_some());
        assert!(crate::Cli::try_parse_from(["thegn", "skills", "show", "mq"]).is_ok());
        assert!(
            crate::Cli::try_parse_from([
                "thegn",
                "skills",
                "seed",
                "--worktree",
                "/tmp/w",
                "--json"
            ])
            .is_ok()
        );
    }

    #[test]
    fn embedded_list_metadata_is_deterministic() {
        let loaded = crate::skill_seed::load_registry(&Config::default());
        let names: Vec<&str> = loaded.registry.iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["mq", "pipeline", "supervise"]);
        assert!(loaded.diagnostics.is_empty());
    }
}
