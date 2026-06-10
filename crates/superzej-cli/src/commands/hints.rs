use crate::config::Config;
use anyhow::Result;
use std::collections::BTreeMap;

pub fn run(cfg: &Config) -> Result<()> {
    // Map tool command/name to its hints
    let mut map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

    for tool in &cfg.tools {
        if !tool.hints.is_empty() {
            let hints: Vec<(String, String)> = tool
                .hints
                .iter()
                .map(|h| (h.key.clone(), h.label.clone()))
                .collect();
            // Store by name
            map.insert(tool.name.clone(), hints.clone());
            // Store by first word of command (if different from name)
            let cmd_bin = tool
                .command
                .split_whitespace()
                .next()
                .unwrap_or(&tool.command);
            if cmd_bin != tool.name && !cmd_bin.is_empty() {
                map.insert(cmd_bin.to_string(), hints);
            }
        }
    }

    // Add default fallbacks if not explicitly configured so users don't lose the existing UX
    let mut ensure_default = |name: &str, defaults: &[(&str, &str)]| {
        if !map.contains_key(name) {
            map.insert(
                name.to_string(),
                defaults
                    .iter()
                    .map(|(k, l)| (k.to_string(), l.to_string()))
                    .collect(),
            );
        }
    };

    ensure_default(
        "lazygit",
        &[("q", "quit"), ("Space", "commit"), ("?", "help")],
    );
    ensure_default("yazi", &[("q", "quit"), ("/", "search"), ("Enter", "open")]);
    ensure_default("hx", &[(":w", "save"), (":q", "quit")]);
    ensure_default("vim", &[(":w", "save"), (":q", "quit")]);

    let json = serde_json::to_string(&map)?;
    println!("{}", json);
    Ok(())
}
