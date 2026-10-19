use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub title: String,
    pub description: Option<String>,
    /// Parsed for schema compatibility; the host's `[observe]` cadence is used.
    pub refresh: Option<String>,
    pub panels: Vec<Panel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Panel {
    pub id: u32,
    pub title: String,
    #[serde(rename = "type")]
    pub panel_type: String, // "timeseries", "stat", "table", "logs"
    /// Which datasource this panel queries ("host", "prometheus", "loki"). Empty
    /// is accepted only when exactly one datasource is configured.
    #[serde(default)]
    pub datasource: String,
    pub grid_pos: GridPos,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridPos {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub expr: String,
    pub ref_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide: bool,
}

/// The zero-config dashboard: live host CPU / memory / load, sampled locally by
/// the `host` datasource. Shown when `[observe] dashboard_path` is empty, so the
/// Observe tab is useful with no external Prometheus/Loki. Uses the standard
/// 24-column Grafana-style grid.
pub fn builtin_host_dashboard() -> Dashboard {
    let panel = |id, title: &str, kind: &str, expr: &str, (x, y, w, h)| Panel {
        id,
        title: title.to_string(),
        panel_type: kind.to_string(),
        datasource: "host".to_string(),
        grid_pos: GridPos { x, y, w, h },
        targets: vec![Target {
            ref_id: "A".to_string(),
            expr: expr.to_string(),
            ..Target::default()
        }],
    };
    Dashboard {
        title: "Host".to_string(),
        description: Some("Live host metrics (no external datasource)".to_string()),
        refresh: Some("15s".to_string()),
        panels: vec![
            panel(1, "CPU %", "timeseries", "host_cpu_pct", (0, 0, 16, 8)),
            panel(2, "CPU now", "stat", "host_cpu_pct", (16, 0, 8, 4)),
            panel(3, "Mem used (GiB)", "stat", "host_mem_used", (16, 4, 8, 4)),
            panel(4, "Load (1m)", "timeseries", "host_load1", (0, 8, 24, 8)),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_host_dashboard_is_well_formed() {
        let d = builtin_host_dashboard();
        assert_eq!(d.panels.len(), 4);
        // ids unique
        let mut ids: Vec<u32> = d.panels.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 4);
        // every panel fits the 24-column grid and targets the host source
        for p in &d.panels {
            assert!(
                p.grid_pos.x + p.grid_pos.w <= 24,
                "panel {} overflows grid",
                p.id
            );
            assert_eq!(p.datasource, "host");
            assert!(!p.targets.is_empty());
        }
    }

    #[test]
    fn panel_datasource_defaults_to_empty() {
        let d: Dashboard = toml::from_str(
            r#"
title = "T"
[[panels]]
id = 1
title = "P"
type = "stat"
[panels.grid_pos]
x = 0
y = 0
w = 4
h = 4
[[panels.targets]]
ref_id = "A"
expr = "x"
"#,
        )
        .unwrap();
        assert_eq!(d.panels[0].datasource, "");
    }

    #[test]
    fn test_dashboard_toml_serde() {
        let toml_str = r#"
title = "Overview"
refresh = "5s"

[[panels]]
id = 1
title = "CPU Usage"
type = "timeseries"

[panels.grid_pos]
x = 0
y = 0
w = 12
h = 8

[[panels.targets]]
ref_id = "A"
expr = "cpu_usage"
"#;

        let dashboard: Dashboard = toml::from_str(toml_str).unwrap();
        assert_eq!(dashboard.title, "Overview");
        assert_eq!(dashboard.refresh, Some("5s".to_string()));
        assert_eq!(dashboard.panels.len(), 1);

        let p = &dashboard.panels[0];
        assert_eq!(p.title, "CPU Usage");
        assert_eq!(p.panel_type, "timeseries");
        assert_eq!(p.grid_pos.w, 12);
        assert_eq!(p.targets[0].expr, "cpu_usage");
        assert_eq!(p.targets[0].alias, None);
        assert!(!p.targets[0].hide);
    }

    #[test]
    fn target_alias_and_hide_round_trip_and_default_fields_are_omitted() {
        let dashboard: Dashboard = toml::from_str(
            r#"
title = "T"
[[panels]]
id = 1
title = "P"
type = "stat"
[panels.grid_pos]
x = 0
y = 0
w = 4
h = 4
[[panels.targets]]
ref_id = "A"
expr = "x"
alias = "p99"
hide = true
"#,
        )
        .unwrap();
        let target = &dashboard.panels[0].targets[0];
        assert_eq!(target.alias.as_deref(), Some("p99"));
        assert!(target.hide);

        let serialized = toml::to_string(target).unwrap();
        let round_trip: Target = toml::from_str(&serialized).unwrap();
        assert_eq!(round_trip, *target);

        let omitted: Dashboard = toml::from_str(
            r#"
title = "T"
[[panels]]
id = 1
title = "P"
type = "stat"
[panels.grid_pos]
x = 0
y = 0
w = 4
h = 4
[[panels.targets]]
ref_id = "A"
expr = "x"
"#,
        )
        .unwrap();
        let target = &omitted.panels[0].targets[0];
        assert_eq!(target.alias, None);
        assert!(!target.hide);

        let serialized_default = toml::to_string(&Target::default()).unwrap();
        assert!(!serialized_default.contains("alias"));
        assert!(!serialized_default.contains("hide"));
    }
}
