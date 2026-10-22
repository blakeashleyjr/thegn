//! Read-only status for the Linux standalone install's batteries components.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;

const REMEDIATION: &str = "from a thegn source checkout run ./install.sh --batteries (the release archive does not include it), or nix run github:blakeashleyjr/thegn#batteries";
const MARKER_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatteriesEnv {
    pub os: &'static str,
    pub data_home: PathBuf,
    pub config_home: PathBuf,
    pub path_alacritty: Option<PathBuf>,
    pub alacritty_config_env: Option<PathBuf>,
    pub tg_on_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Present,
    Missing,
    NotInstalled,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Component {
    pub name: String,
    pub state: ComponentState,
    pub detail: String,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatteriesReport {
    pub terminal: Component,
    pub font: Component,
    pub alacritty_config: Component,
    pub launcher: Component,
}

/// Inspect the current process environment without invoking external tools.
pub fn inspect_env() -> BatteriesReport {
    let home = thegn_core::util::home();
    let data_home = absolute_env_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
    let config_home = absolute_env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    let env = BatteriesEnv {
        os: std::env::consts::OS,
        data_home,
        config_home,
        path_alacritty: find_on_path("alacritty"),
        alacritty_config_env: std::env::var_os("THEGN_ALACRITTY_CONFIG").map(PathBuf::from),
        tg_on_path: find_on_path("tg"),
    };
    inspect_with(&env)
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file() && is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| {
        path.is_file() && crate::platform::gate_path::executable_bit(&metadata) != Some(false)
    })
}

fn inspect_with(env: &BatteriesEnv) -> BatteriesReport {
    if env.os != "linux" {
        return BatteriesReport {
            terminal: component(
                "terminal",
                ComponentState::Unsupported,
                "Linux-only (macOS batteries deferred)",
                None,
            ),
            font: component(
                "font",
                ComponentState::Unsupported,
                "Linux-only (macOS batteries deferred)",
                None,
            ),
            alacritty_config: component(
                "alacritty_config",
                ComponentState::Unsupported,
                "Linux-only (macOS batteries deferred)",
                None,
            ),
            launcher: component(
                "launcher",
                ComponentState::Unsupported,
                "Linux-only (macOS batteries deferred)",
                None,
            ),
        };
    }

    let marker_path = env.data_home.join("thegn/batteries/marker");
    let marker_exists = marker_path.is_file();
    let (marker, marker_error) = read_marker(&marker_path);
    let remediation = Some(REMEDIATION.to_string());

    let terminal_path = marker
        .get("alacritty")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file() && is_executable(path));
    let terminal = if let Some(path) = terminal_path {
        component(
            "terminal",
            ComponentState::Present,
            format!("Alacritty at {}", path.display()),
            None,
        )
    } else if let Some(path) = env
        .path_alacritty
        .as_ref()
        .filter(|path| path.is_file() && is_executable(path))
    {
        component(
            "terminal",
            ComponentState::Present,
            format!("host Alacritty at {}", path.display()),
            None,
        )
    } else {
        absent_component(
            "terminal",
            marker_exists,
            marker_error
                .as_deref()
                .unwrap_or("Alacritty executable not found"),
            remediation.clone(),
        )
    };

    let font_path = marker.get("font_dir").map(PathBuf::from);
    let font_files = font_path.as_deref().and_then(ttf_in_dir);
    let font = match font_files {
        Some(path) => component(
            "font",
            ComponentState::Present,
            format!("FiraCode Nerd Font in {}", path.display()),
            None,
        ),
        None => absent_component(
            "font",
            marker_exists,
            marker_error
                .as_deref()
                .unwrap_or("FiraCode Nerd Font files not found"),
            remediation.clone(),
        ),
    };

    let config_path = env
        .alacritty_config_env
        .clone()
        .or_else(|| marker.get("alacritty_config").map(PathBuf::from))
        .unwrap_or_else(|| env.config_home.join("thegn/alacritty.toml"));
    let config = if config_path.is_file() {
        if writable_without_writing(&config_path) {
            component(
                "alacritty_config",
                ComponentState::Present,
                format!("writable {}", config_path.display()),
                None,
            )
        } else {
            component(
                "alacritty_config",
                ComponentState::Missing,
                format!("{} is not writable", config_path.display()),
                remediation.clone(),
            )
        }
    } else if config_path.exists() {
        component(
            "alacritty_config",
            ComponentState::Missing,
            format!("{} is not a regular file", config_path.display()),
            remediation.clone(),
        )
    } else {
        absent_component(
            "alacritty_config",
            marker_exists,
            marker_error
                .as_deref()
                .unwrap_or("writable Alacritty config not found"),
            remediation.clone(),
        )
    };

    let launcher = match env
        .tg_on_path
        .as_ref()
        .filter(|path| path.is_file() && is_executable(path))
    {
        Some(path) => component(
            "launcher",
            ComponentState::Present,
            format!("tg at {}", path.display()),
            None,
        ),
        None => absent_component(
            "launcher",
            marker_exists,
            marker_error
                .as_deref()
                .unwrap_or("tg launcher not found on PATH"),
            remediation,
        ),
    };

    BatteriesReport {
        terminal,
        font,
        alacritty_config: config,
        launcher,
    }
}

fn read_marker(path: &Path) -> (BTreeMap<String, String>, Option<String>) {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (BTreeMap::new(), None),
        Err(error) => {
            return (
                BTreeMap::new(),
                Some(format!("batteries marker unreadable: {error}")),
            );
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.by_ref().take(MARKER_LIMIT + 1).read_to_end(&mut bytes) {
        return (
            BTreeMap::new(),
            Some(format!("batteries marker unreadable: {error}")),
        );
    }
    if bytes.len() as u64 > MARKER_LIMIT {
        return (
            BTreeMap::new(),
            Some("batteries marker exceeds 64 KiB".to_string()),
        );
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            return (
                BTreeMap::new(),
                Some("batteries marker is malformed".to_string()),
            );
        }
    };
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return (
                BTreeMap::new(),
                Some("batteries marker is malformed".to_string()),
            );
        };
        if key.is_empty() {
            return (
                BTreeMap::new(),
                Some("batteries marker is malformed".to_string()),
            );
        }
        if matches!(
            key,
            "version" | "alacritty" | "font_dir" | "alacritty_config"
        ) && values.insert(key.to_string(), value.to_string()).is_some()
        {
            return (
                BTreeMap::new(),
                Some("batteries marker is malformed".to_string()),
            );
        }
    }
    if values.get("version").map(String::as_str) != Some("1")
        || ["alacritty", "font_dir", "alacritty_config"]
            .iter()
            .any(|key| !values.contains_key(*key))
    {
        return (
            BTreeMap::new(),
            Some("batteries marker is malformed".to_string()),
        );
    }
    (values, None)
}

fn ttf_in_dir(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|extension| extension == "ttf")
                && path.is_file()
                && !path.is_symlink()
        })
}

fn writable_without_writing(path: &Path) -> bool {
    OpenOptions::new().append(true).open(path).is_ok()
}

fn component(
    name: &str,
    state: ComponentState,
    detail: impl Into<String>,
    remediation: Option<String>,
) -> Component {
    Component {
        name: name.to_string(),
        state,
        detail: detail.into(),
        remediation,
    }
}

fn absent_component(
    name: &str,
    marker_exists: bool,
    detail: &str,
    remediation: Option<String>,
) -> Component {
    let state = if marker_exists {
        ComponentState::Missing
    } else {
        ComponentState::NotInstalled
    };
    component(name, state, detail, remediation)
}

pub fn print(report: &BatteriesReport) {
    for line in lines(report) {
        thegn_core::outln!("{line}");
    }
}

fn lines(report: &BatteriesReport) -> Vec<String> {
    let mut lines = vec!["Batteries (terminal, font, config, launcher)".to_string()];
    for item in [
        &report.terminal,
        &report.font,
        &report.alacritty_config,
        &report.launcher,
    ] {
        let state = match &item.state {
            ComponentState::Present => "present",
            ComponentState::Missing => "MISSING",
            ComponentState::NotInstalled => "not installed",
            ComponentState::Unsupported => "unsupported",
        };
        let label = if item.name == "alacritty_config" {
            "alacritty config"
        } else {
            item.name.as_str()
        };
        lines.push(format!("  {label}  {state} — {}", item.detail));
        if let Some(fix) = &item.remediation {
            lines.push(format!("    fix: {fix}"));
        }
    }
    lines
}

pub fn json(report: &BatteriesReport) -> serde_json::Value {
    serde_json::to_value(report).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::{BatteriesEnv, ComponentState, REMEDIATION, inspect_with, lines};
    use std::fs;

    fn write_executable(path: &std::path::Path) {
        fs::copy(std::env::current_exe().expect("test executable path"), path)
            .expect("copy executable fixture");
    }

    fn env(root: &std::path::Path) -> BatteriesEnv {
        BatteriesEnv {
            os: "linux",
            data_home: root.join("data"),
            config_home: root.join("config"),
            path_alacritty: None,
            alacritty_config_env: None,
            tg_on_path: None,
        }
    }

    fn marker(
        env: &BatteriesEnv,
        alacritty: &std::path::Path,
        font_dir: &std::path::Path,
        config: &std::path::Path,
    ) {
        let path = env.data_home.join("thegn/batteries/marker");
        fs::create_dir_all(path.parent().expect("marker parent")).expect("create marker parent");
        fs::write(
            path,
            format!(
                "version=1\nalacritty={}\nfont_dir={}\nalacritty_config={}\nunknown=ignored\n",
                alacritty.display(),
                font_dir.display(),
                config.display()
            ),
        )
        .expect("write marker");
    }

    #[test]
    fn absent_marker_reports_not_installed_with_remediation() {
        let root = tempfile::tempdir().expect("tempdir");
        let report = inspect_with(&env(root.path()));
        for item in [
            &report.terminal,
            &report.font,
            &report.alacritty_config,
            &report.launcher,
        ] {
            assert_eq!(item.state, ComponentState::NotInstalled);
            assert!(
                item.remediation
                    .as_deref()
                    .is_some_and(|fix| fix.contains("./install.sh --batteries"))
            );
        }
    }

    #[test]
    fn remediation_names_the_source_checkout() {
        assert!(REMEDIATION.contains("source checkout"));
        assert!(REMEDIATION.contains("release archive"));
    }

    #[test]
    fn marker_with_missing_font_dir_reports_font_missing() {
        let root = tempfile::tempdir().expect("tempdir");
        let fixture = env(root.path());
        let bin = std::env::current_exe().expect("test executable path");
        let config = root.path().join("alacritty.toml");
        fs::write(&config, "font = {}\n").expect("write config");
        marker(&fixture, &bin, &root.path().join("missing-fonts"), &config);
        let report = inspect_with(&fixture);
        assert_eq!(report.terminal.state, ComponentState::Present);
        assert_eq!(report.font.state, ComponentState::Missing);
        assert!(report.font.remediation.is_some());
    }

    #[test]
    fn marker_with_ttf_reports_font_present() {
        let root = tempfile::tempdir().expect("tempdir");
        let fixture = env(root.path());
        let bin = std::env::current_exe().expect("test executable path");
        let fonts = root.path().join("fonts");
        fs::create_dir_all(&fonts).expect("create fonts");
        fs::write(fonts.join("FiraCode.ttf"), "font").expect("write font");
        let config = root.path().join("alacritty.toml");
        fs::write(&config, "font = {}\n").expect("write config");
        marker(&fixture, &bin, &fonts, &config);
        let launcher = root.path().join("tg");
        write_executable(&launcher);
        let mut ready = fixture.clone();
        ready.tg_on_path = Some(launcher);
        let report = inspect_with(&ready);
        assert_eq!(report.terminal.state, ComponentState::Present);
        assert_eq!(report.font.state, ComponentState::Present);
        assert_eq!(report.alacritty_config.state, ComponentState::Present);
        assert_eq!(report.launcher.state, ComponentState::Present);
    }

    #[test]
    fn unwritable_alacritty_config_is_reported_with_fix() {
        let root = tempfile::tempdir().expect("tempdir");
        let fixture = env(root.path());
        let config = root.path().join("readonly.toml");
        fs::write(&config, "config").expect("write config");
        let mut permissions = fs::metadata(&config).expect("metadata").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&config, permissions).expect("set read-only");
        let mut updated = fixture.clone();
        updated.alacritty_config_env = Some(config.clone());
        if super::writable_without_writing(&config) {
            thegn_core::outln!("read-only fixture remains writable; skipping permission assertion");
            return;
        }
        let item = inspect_with(&updated).alacritty_config;
        assert_eq!(item.state, ComponentState::Missing);
        assert_eq!(item.detail, format!("{} is not writable", config.display()));
        assert!(item.remediation.is_some());
    }

    #[test]
    fn report_lines_include_missing_components_and_remediation_only_for_actionable_states() {
        let root = tempfile::tempdir().expect("tempdir");
        let fixture = env(root.path());
        let bin = std::env::current_exe().expect("test executable path");
        let config = root.path().join("alacritty.toml");
        fs::write(&config, "config").expect("write config");
        marker(&fixture, &bin, &root.path().join("fonts"), &config);
        let report = inspect_with(&fixture);
        let rendered = lines(&report);
        assert!(rendered.contains(&format!(
            "  terminal  present — Alacritty at {}",
            bin.display()
        )));
        assert!(
            rendered.contains(&"  font  MISSING — FiraCode Nerd Font files not found".to_string())
        );
        assert!(rendered.contains(&format!(
            "  alacritty config  present — writable {}",
            config.display()
        )));
        assert!(
            rendered.contains(&"  launcher  MISSING — tg launcher not found on PATH".to_string())
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.starts_with("    fix: "))
                .count(),
            2
        );
        assert!(
            rendered
                .iter()
                .all(|line| !line.starts_with("  color ") && !line.starts_with("  glyphs "))
        );
    }

    #[test]
    fn marker_present_report_prints_every_missing_component_and_its_fix() {
        let root = tempfile::tempdir().expect("tempdir");
        let fixture = env(root.path());
        marker(
            &fixture,
            &root.path().join("missing-alacritty"),
            &root.path().join("missing-fonts"),
            &root.path().join("missing-config"),
        );
        let rendered = lines(&inspect_with(&fixture));
        assert!(
            rendered.contains(&"  terminal  MISSING — Alacritty executable not found".to_string())
        );
        assert!(
            rendered.contains(&"  font  MISSING — FiraCode Nerd Font files not found".to_string())
        );
        assert!(rendered.contains(
            &"  alacritty config  MISSING — writable Alacritty config not found".to_string()
        ));
        assert!(
            rendered.contains(&"  launcher  MISSING — tg launcher not found on PATH".to_string())
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.starts_with("    fix: "))
                .count(),
            4
        );
        assert!(
            rendered
                .iter()
                .filter(|line| line.starts_with("    fix: "))
                .all(|line| line == &format!("    fix: {}", super::REMEDIATION))
        );
        assert!(
            rendered
                .iter()
                .all(|line| !line.starts_with("  color ") && !line.starts_with("  glyphs "))
        );
    }

    #[test]
    fn malformed_marker_is_reported_not_panicking() {
        let root = tempfile::tempdir().expect("tempdir");
        let fixture = env(root.path());
        let path = fixture.data_home.join("thegn/batteries/marker");
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, "not-a-key-value\n").expect("write malformed marker");
        let report = inspect_with(&fixture);
        assert!(report.terminal.detail.contains("malformed"));
        assert_eq!(report.font.state, ComponentState::Missing);
    }

    #[test]
    fn non_linux_reports_unsupported_for_every_component() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut fixture = env(root.path());
        fixture.os = "macos";
        let report = inspect_with(&fixture);
        for item in [
            &report.terminal,
            &report.font,
            &report.alacritty_config,
            &report.launcher,
        ] {
            assert_eq!(item.state, ComponentState::Unsupported);
            assert_eq!(item.detail, "Linux-only (macOS batteries deferred)");
        }
    }

    #[test]
    fn json_has_one_entry_per_component() {
        let root = tempfile::tempdir().expect("tempdir");
        let value = super::json(&inspect_with(&env(root.path())));
        assert_eq!(value.as_object().expect("object").len(), 4);
        for key in ["terminal", "font", "alacritty_config", "launcher"] {
            assert!(value.get(key).is_some());
        }
    }
}
