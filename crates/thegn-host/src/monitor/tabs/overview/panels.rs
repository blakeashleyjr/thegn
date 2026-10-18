use super::*;
use std::fmt::Write as _;

pub(super) struct PanelText {
    pub(super) badge: String,
    pub(super) lines: [String; 3],
    pub(super) line_count: usize,
    pub(super) tone: StateRole,
}

impl Default for PanelText {
    fn default() -> Self {
        Self {
            badge: String::new(),
            lines: std::array::from_fn(|_| String::new()),
            line_count: 0,
            tone: StateRole::Ok,
        }
    }
}

impl PanelText {
    pub(super) fn clear(&mut self) {
        self.badge.clear();
        for line in &mut self.lines {
            line.clear();
        }
        self.line_count = 0;
        self.tone = StateRole::Ok;
    }
}

impl OverviewView<'_> {
    pub(super) fn panel_content_into(&self, index: usize, out: &mut PanelText) {
        out.clear();
        let model = self.0;
        match index {
            0 => {
                let cores = &model.cpu_cores;
                let first = &mut out.lines[0];
                if let Some(value) = model.stats.cpu_pct {
                    write!(first, "total  {value}%").expect("write to String");
                } else {
                    write!(first, "total  {}", absent()).expect("write to String");
                }
                let second = &mut out.lines[1];
                second.push_str("cores  ");
                if cores.is_empty() {
                    second.push_str(absent());
                } else {
                    for (position, busy) in cores.iter().enumerate() {
                        if position > 0 {
                            second.push_str("  ");
                        }
                        write!(second, "{}%", busy / 10).expect("write to String");
                    }
                }
                out.line_count = 2;
            }
            1 => {
                let load = model
                    .load
                    .map(|v| {
                        (
                            f64::from(v.load1) / 100.0,
                            f64::from(v.load5) / 100.0,
                            f64::from(v.load15) / 100.0,
                        )
                    })
                    .or_else(|| {
                        model
                            .stats
                            .load_avg
                            .map(|v| (f64::from(v.0), f64::from(v.1), f64::from(v.2)))
                    });
                let line = &mut out.lines[0];
                line.push_str("load  ");
                if let Some((one, five, fifteen)) = load {
                    write!(line, "{one:.2}  {five:.2}  {fifteen:.2}").expect("write to String");
                } else {
                    line.push_str(absent());
                }
                write_pressure("memory PSI  ", &model.psi.memory, &mut out.lines[1]);
                write_pressure("CPU PSI  ", &model.psi.cpu, &mut out.lines[2]);
                out.line_count = 3;
            }
            2 => {
                let used = model
                    .mem_total
                    .zip(model.mem_available)
                    .map(|(total, available)| total.saturating_sub(available))
                    .map(format_bytes)
                    .or_else(|| {
                        model
                            .stats
                            .mem_gib
                            .map(|v| format_bytes((f64::from(v.0) * 1_073_741_824.0) as u64))
                    });
                if let Some(used) = used {
                    write!(out.lines[0], "used  {used}").expect("write to String");
                } else {
                    write!(out.lines[0], "used  {}", absent()).expect("write to String");
                }
                if let Some(available) = model.mem_available {
                    write!(out.lines[1], "available  {}", format_bytes(available))
                        .expect("write to String");
                } else if let Some(free) = model.stats.mem_free_gib {
                    write!(
                        out.lines[1],
                        "free  {}",
                        format_bytes((f64::from(free) * 1_073_741_824.0) as u64)
                    )
                    .expect("write to String");
                } else {
                    write!(out.lines[1], "available  {}", absent()).expect("write to String");
                }
                let swap = model
                    .swap_used
                    .or_else(|| {
                        model
                            .stats
                            .swap_gib
                            .map(|v| (f64::from(v.0) * 1_073_741_824.0) as u64)
                    })
                    .map(format_bytes);
                if let Some(swap) = swap {
                    write!(out.lines[2], "swap  {swap}").expect("write to String");
                } else {
                    write!(out.lines[2], "swap  {}", absent()).expect("write to String");
                }
                let health = model.health();
                out.tone = match health.verdict {
                    mem_health::Verdict::Oom | mem_health::Verdict::Thrashing => StateRole::Crit,
                    mem_health::Verdict::Pressure | mem_health::Verdict::Tight => StateRole::Warn,
                    mem_health::Verdict::Unknown => StateRole::Unknown,
                    mem_health::Verdict::Ok => StateRole::Ok,
                };
                write!(out.badge, "health  {}", health.verdict.label()).expect("write to String");
                out.line_count = 3;
            }
            3 => {
                write_rate("rx  ", model.stats.net_bps.map(|v| v.0), &mut out.lines[0]);
                write_rate("tx  ", model.stats.net_bps.map(|v| v.1), &mut out.lines[1]);
                write!(
                    out.lines[2],
                    "interfaces  {}",
                    model.net_ifaces.max(model.stats.net_ifaces.len())
                )
                .expect("write to String");
                out.line_count = 3;
            }
            4 => {
                write_rate(
                    "read  ",
                    model
                        .disk_read_bps
                        .map(|v| v.max(0.0) as u64)
                        .or_else(|| model.stats.disk_io.map(|v| v.0)),
                    &mut out.lines[0],
                );
                write_rate(
                    "write  ",
                    model
                        .disk_write_bps
                        .map(|v| v.max(0.0) as u64)
                        .or_else(|| model.stats.disk_io.map(|v| v.1)),
                    &mut out.lines[1],
                );
                if let Some(pct) = model.stats.disk_free_pct {
                    write!(out.lines[2], "root free  {pct}%").expect("write to String");
                } else {
                    write!(out.lines[2], "root free  {}", absent()).expect("write to String");
                }
                out.line_count = 3;
            }
            5 => {
                let hottest = model.hottest_c.or(model.stats.cpu_temp_c);
                if let Some(value) = hottest {
                    let formatted = kit::edge_fmt(NumFmt::default()).reading(
                        Reading::Value(f64::from(value)),
                        Unit::Temp,
                        8,
                    );
                    write!(out.lines[0], "hottest  {formatted}").expect("write to String");
                } else {
                    write!(out.lines[0], "hottest  {}", absent()).expect("write to String");
                }
                write!(out.lines[1], "zones  {}", model.thermal_zones).expect("write to String");
                out.line_count = 2;
            }
            6 => {
                let charge = model
                    .battery
                    .and_then(|battery| battery.0)
                    .or_else(|| model.stats.battery.map(|value| value.0));
                if let Some(charge) = charge {
                    write!(out.lines[0], "charge  {charge}%").expect("write to String");
                } else {
                    write!(out.lines[0], "charge  {}", absent()).expect("write to String");
                }
                if let Some(status) = model.battery.and_then(|battery| battery.1) {
                    let label = match status {
                        thegn_metrics::collect::model::power_supply::SupplyStatus::Charging => {
                            "charging"
                        }
                        thegn_metrics::collect::model::power_supply::SupplyStatus::Discharging => {
                            "discharging"
                        }
                        thegn_metrics::collect::model::power_supply::SupplyStatus::NotCharging => {
                            "not charging"
                        }
                        thegn_metrics::collect::model::power_supply::SupplyStatus::Full => "full",
                        thegn_metrics::collect::model::power_supply::SupplyStatus::Unknown => {
                            "unknown"
                        }
                    };
                    write!(out.lines[1], "state  {label}").expect("write to String");
                } else if let Some((_, ac)) = model.stats.battery {
                    write!(out.lines[1], "state  {}", if ac { "AC" } else { "battery" })
                        .expect("write to String");
                } else {
                    write!(out.lines[1], "state  {}", absent()).expect("write to String");
                }
                out.line_count = 2;
            }
            _ => {
                write!(out.lines[0], "containers  {}", model.container_count)
                    .expect("write to String");
                write!(
                    out.lines[1],
                    "child processes  {}",
                    model.stats.children_count()
                )
                .expect("write to String");
                if let Some(running) = model.procs_running {
                    write!(out.lines[2], "running  {running}").expect("write to String");
                } else {
                    write!(out.lines[2], "running  {}", absent()).expect("write to String");
                }
                out.line_count = 3;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn panel_content(&self, index: usize) -> (Option<String>, Vec<String>, StateRole) {
        let mut content = PanelText::default();
        self.panel_content_into(index, &mut content);
        (
            (!content.badge.is_empty()).then_some(content.badge),
            content.lines[..content.line_count].to_vec(),
            content.tone,
        )
    }
}

fn write_pressure(
    prefix: &str,
    resource: &thegn_metrics::collect::model::psi::PsiResource,
    out: &mut String,
) {
    out.push_str(prefix);
    out.push_str("some ");
    if let Some(value) = resource.file.some {
        write!(out, "{:.2}%", f64::from(value.avg10) / 100.0).expect("write to String");
    } else {
        out.push_str(absent());
    }
    out.push_str("  full ");
    if let Some(value) = resource.file.full {
        write!(out, "{:.2}%", f64::from(value.avg10) / 100.0).expect("write to String");
    } else {
        out.push_str(absent());
    }
}

fn write_rate(prefix: &str, rate: Option<u64>, out: &mut String) {
    out.push_str(prefix);
    if let Some(rate) = rate {
        write!(out, "{}/s", format_bytes(rate)).expect("write to String");
    } else {
        out.push_str(absent());
    }
}

fn absent() -> &'static str {
    crate::caps::active_glyphs().emdash
}

fn format_bytes(value: u64) -> String {
    kit::edge_fmt(NumFmt::default()).reading(Reading::Value(value as f64), Unit::Bytes, 10)
}
