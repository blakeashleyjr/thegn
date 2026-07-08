use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub title: String,
    pub description: Option<String>,
    pub refresh: Option<String>, // e.g. "5s", "1m"
    pub panels: Vec<Panel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Panel {
    pub id: u32,
    pub title: String,
    #[serde(rename = "type")]
    pub panel_type: String, // "timeseries", "stat", "table", "logs"
    /// Which datasource this panel queries ("host", "prometheus", "loki"). Empty
    /// ⇒ the engine's default (first registered) source.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub expr: String,
    pub ref_id: String,
}

/// The zero-config dashboard: live host CPU / memory / load, sampled locally by
/// the `host` datasource. Shown when `[observe] dashboard_path` is empty (or a
/// custom dashboard fails to load), so the Observe tab is useful with no external
/// Prometheus/Loki. Uses the standard 24-column Grafana-style grid.
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
    }
}
