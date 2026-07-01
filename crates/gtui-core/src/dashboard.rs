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

#[cfg(test)]
mod tests {
    use super::*;

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
