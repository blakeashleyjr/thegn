use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::outln;

pub(crate) struct ExposureInput<'a> {
    pub runtime_dir: Option<&'a Path>,
    pub home: &'a Path,
    pub mounts: &'a [(String, String)],
}

fn mount_host(spec: &str, home: &Path) -> PathBuf {
    let host = spec.split(':').next().unwrap_or(spec);
    match host.strip_prefix('~') {
        Some(rest) if !home.as_os_str().is_empty() => {
            home.join(rest.strip_prefix('/').unwrap_or(rest))
        }
        _ => PathBuf::from(host),
    }
}

fn covers_login_runtime_dir(path: &Path) -> bool {
    Path::new("/run/user").starts_with(path) || path.starts_with("/run/user")
}

fn covers_mounts<'a>(input: &ExposureInput<'a>) -> Vec<(&'a str, &'a str)> {
    input
        .mounts
        .iter()
        .filter_map(|(table, spec)| {
            covers_login_runtime_dir(&mount_host(spec, input.home))
                .then_some((table.as_str(), spec.as_str()))
        })
        .collect()
}

pub(crate) fn warnings(input: &ExposureInput<'_>) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(runtime_dir) = input.runtime_dir
        && !runtime_dir.starts_with("/run/user/")
        && let Some((table, spec)) = covers_mounts(input).into_iter().next()
    {
        warnings.push(format!(
            "sandboxes mask the bus, systemd, podman and thegn control sockets only under XDG_RUNTIME_DIR={}; mount {table}:{spec} exposes the login runtime dir unmasked (start this instance without overriding XDG_RUNTIME_DIR)",
            runtime_dir.display()
        ));
    }
    if let Some(runtime_dir) = input.runtime_dir
        && !input.home.as_os_str().is_empty()
        && runtime_dir.starts_with(input.home)
    {
        warnings.push(format!(
            "this daemon's control socket lives under $HOME ({}); sandboxes of other instances that bind $HOME can reach it",
            runtime_dir.display()
        ));
    }
    warnings
}

fn configured_mounts(cfg: &Config) -> Vec<(String, String)> {
    let mut mounts = cfg
        .sandbox
        .mounts
        .iter()
        .cloned()
        .map(|mount| ("sandbox".to_string(), mount))
        .collect::<Vec<_>>();
    for (name, env) in &cfg.env {
        if let Some(env_mounts) = &env.sandbox.mounts {
            mounts.extend(
                env_mounts
                    .iter()
                    .cloned()
                    .map(|mount| (format!("env.{name}"), mount)),
            );
        }
    }
    mounts
}

fn canonicalize_best_effort(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn config_input(cfg: &Config) -> (Option<PathBuf>, PathBuf, Vec<(String, String)>) {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let home = canonicalize_best_effort(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
    );
    let runtime_dir = runtime_dir.map(canonicalize_best_effort);
    let mounts = configured_mounts(cfg);
    (runtime_dir, home, mounts)
}

pub(crate) fn json(cfg: &Config) -> serde_json::Value {
    let (runtime_dir, home, mounts) = config_input(cfg);
    let input = ExposureInput {
        runtime_dir: runtime_dir.as_deref(),
        home: &home,
        mounts: &mounts,
    };
    let runtime_dir_text = runtime_dir.as_ref().map(|path| path.display().to_string());
    let run_user_mounts = covers_mounts(&input)
        .into_iter()
        .map(|(table, mount)| serde_json::json!({ "table": table, "mount": mount }))
        .collect::<Vec<_>>();
    serde_json::json!({
        "runtime_dir": runtime_dir_text,
        "run_user_mounts": run_user_mounts,
        "warnings": warnings(&input),
    })
}

pub(crate) fn report(cfg: &Config) {
    let (runtime_dir, home, mounts) = config_input(cfg);
    let input = ExposureInput {
        runtime_dir: runtime_dir.as_deref(),
        home: &home,
        mounts: &mounts,
    };
    let warnings = warnings(&input);
    if warnings.is_empty() {
        outln!("  runtime exposure  ok");
    } else {
        for warning in warnings {
            outln!("  runtime exposure  {warning}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        runtime_dir: Option<&'a str>,
        home: &'a str,
        mounts: &'a [(String, String)],
    ) -> ExposureInput<'a> {
        ExposureInput {
            runtime_dir: runtime_dir.map(Path::new),
            home: Path::new(home),
            mounts,
        }
    }

    #[test]
    fn exposure_warns_when_runtime_dir_is_overridden_and_run_user_is_mounted() {
        let mounts = vec![("sandbox".into(), "/run/user:/run/user".into())];
        let warnings = warnings(&input(Some("/tmp/thegn-runtime"), "/home/me", &mounts));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("mount sandbox:/run/user:/run/user"));
    }

    #[test]
    fn exposure_warns_on_a_root_mount() {
        let mounts = vec![("sandbox".into(), "/:/host".into())];
        let warnings = warnings(&input(Some("/tmp/rt"), "/home/me", &mounts));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("mount sandbox:/:/host"));
    }

    #[test]
    fn exposure_is_quiet_on_the_login_runtime_dir() {
        let mounts = vec![("sandbox".into(), "/run:/run".into())];
        assert!(warnings(&input(Some("/run/user/1000"), "/home/me", &mounts)).is_empty());
    }

    #[test]
    fn exposure_warns_when_runtime_dir_is_under_home() {
        let warnings = warnings(&input(Some("/home/me/.local/run"), "/home/me", &[]));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("$HOME (/home/me/.local/run)"));
    }

    #[test]
    fn exposure_expands_tilde_and_ignores_unrelated_mounts() {
        let mounts = vec![
            ("env.dev".into(), "~/run:/run".into()),
            ("sandbox".into(), "/tmp:/tmp".into()),
        ];
        let warnings = warnings(&input(Some("/tmp/runtime"), "/run/user/1000", &mounts));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("mount env.dev:~/run:/run"));
    }
}
