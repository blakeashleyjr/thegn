use super::*;

impl OverviewView<'_> {
    pub(super) fn panel_content(&self, index: usize) -> (Option<String>, Vec<String>, StateRole) {
        let model = self.0;
        let lines = match index {
            0 => {
                let cores = model
                    .cpu
                    .cores
                    .iter()
                    .filter_map(|core| core.share.map(|share| format!("{}%", share.busy)))
                    .collect::<Vec<_>>();
                vec![
                    format!("total  {}", pct(model.stats.cpu_pct)),
                    format!(
                        "cores  {}",
                        if cores.is_empty() {
                            absent()
                        } else {
                            cores.join("  ")
                        }
                    ),
                ]
            }
            1 => {
                let load = model
                    .load
                    .value
                    .map(|v| {
                        format!(
                            "{:.2}  {:.2}  {:.2}",
                            f64::from(v.load1) / 100.0,
                            f64::from(v.load5) / 100.0,
                            f64::from(v.load15) / 100.0
                        )
                    })
                    .or_else(|| {
                        model
                            .stats
                            .load_avg
                            .map(|v| format!("{:.2}  {:.2}  {:.2}", v.0, v.1, v.2))
                    })
                    .unwrap_or_else(absent);
                let pressure = |resource: &thegn_metrics::collect::model::psi::PsiResource| {
                    let some = resource
                        .file
                        .some
                        .map(|v| format!("{:.2}%", f64::from(v.avg10) / 100.0))
                        .unwrap_or_else(absent);
                    let full = resource
                        .file
                        .full
                        .map(|v| format!("{:.2}%", f64::from(v.avg10) / 100.0))
                        .unwrap_or_else(absent);
                    format!("some {some}  full {full}")
                };
                vec![
                    format!("load  {load}"),
                    format!("memory PSI  {}", pressure(&model.psi.memory)),
                    format!("CPU PSI  {}", pressure(&model.psi.cpu)),
                ]
            }
            2 => {
                let used = model
                    .mem
                    .used_bytes()
                    .map(format_bytes)
                    .or_else(|| {
                        model
                            .stats
                            .mem_gib
                            .map(|v| format_bytes((f64::from(v.0) * 1_073_741_824.0) as u64))
                    })
                    .unwrap_or_else(absent);
                let available = model
                    .mem
                    .raw
                    .get(thegn_metrics::collect::parse::meminfo::MemField::MemAvailable)
                    .map(|v| format!("available  {}", format_bytes(v)))
                    .unwrap_or_else(|| {
                        model
                            .stats
                            .mem_free_gib
                            .map(|v| {
                                format!(
                                    "free  {}",
                                    format_bytes((f64::from(v) * 1_073_741_824.0) as u64)
                                )
                            })
                            .unwrap_or_else(|| format!("available  {}", absent()))
                    });
                let swap = model
                    .mem
                    .swap_used_bytes()
                    .or_else(|| {
                        model
                            .stats
                            .swap_gib
                            .map(|v| (f64::from(v.0) * 1_073_741_824.0) as u64)
                    })
                    .map(format_bytes)
                    .unwrap_or_else(absent);
                let health = model.health();
                let tone = match health.verdict {
                    mem_health::Verdict::Oom | mem_health::Verdict::Thrashing => StateRole::Crit,
                    mem_health::Verdict::Pressure | mem_health::Verdict::Tight => StateRole::Warn,
                    mem_health::Verdict::Unknown => StateRole::Unknown,
                    mem_health::Verdict::Ok => StateRole::Ok,
                };
                return (
                    Some(format!("health  {:?}", health.verdict)),
                    vec![format!("used  {used}"), available, format!("swap  {swap}")],
                    tone,
                );
            }
            3 => vec![
                format!(
                    "rx  {}/s",
                    model
                        .stats
                        .net_bps
                        .map(|v| format_bytes(v.0))
                        .unwrap_or_else(absent)
                ),
                format!(
                    "tx  {}/s",
                    model
                        .stats
                        .net_bps
                        .map(|v| format_bytes(v.1))
                        .unwrap_or_else(absent)
                ),
                format!(
                    "interfaces  {}",
                    model.net.ifaces.len().max(model.stats.net_ifaces.len())
                ),
            ],
            4 => vec![
                format!(
                    "read  {}/s",
                    model
                        .disk
                        .aggregate
                        .read_bps
                        .map(|v| format_bytes(v.max(0.0) as u64))
                        .or_else(|| model.stats.disk_io.map(|v| format_bytes(v.0)))
                        .unwrap_or_else(absent)
                ),
                format!(
                    "write  {}/s",
                    model
                        .disk
                        .aggregate
                        .write_bps
                        .map(|v| format_bytes(v.max(0.0) as u64))
                        .or_else(|| model.stats.disk_io.map(|v| format_bytes(v.1)))
                        .unwrap_or_else(absent)
                ),
                format!(
                    "root free  {}",
                    model
                        .stats
                        .disk_free_pct
                        .map(|v| format!("{v}%"))
                        .unwrap_or_else(absent)
                ),
            ],
            5 => {
                let hottest = model
                    .thermal
                    .zones
                    .iter()
                    .filter_map(|zone| zone.temp_c)
                    .chain(model.hwmon.values.iter().filter_map(|value| value.value))
                    .reduce(f32::max)
                    .or(model.stats.cpu_temp_c);
                vec![
                    format!(
                        "hottest  {}",
                        hottest
                            .map(|v| kit::edge_fmt(NumFmt::default()).reading(
                                Reading::Value(f64::from(v)),
                                Unit::Temp,
                                8
                            ))
                            .unwrap_or_else(absent)
                    ),
                    format!("zones  {}", model.thermal.zones.len()),
                ]
            }
            6 => {
                let battery = model.power.supplies.iter().find(|supply| {
                    supply.kind == thegn_metrics::collect::model::power_supply::SupplyKind::Battery
                });
                let charge = battery
                    .and_then(|supply| supply.capacity_pct)
                    .or_else(|| model.stats.battery.map(|value| value.0))
                    .map(|value| format!("{value}%"))
                    .unwrap_or_else(absent);
                let state = battery
                    .and_then(|supply| supply.status)
                    .map(|status| format!("{status:?}"))
                    .unwrap_or_else(|| {
                        model
                            .stats
                            .battery
                            .map(|value| {
                                if value.1 {
                                    "AC".into()
                                } else {
                                    "battery".into()
                                }
                            })
                            .unwrap_or_else(absent)
                    });
                vec![format!("charge  {charge}"), format!("state  {state}")]
            }
            _ => vec![
                format!("containers  {}", model.container_count),
                format!("child processes  {}", model.stats.children_count()),
                format!(
                    "running  {}",
                    model
                        .cpu
                        .procs_running
                        .map(|value| value.to_string())
                        .unwrap_or_else(absent)
                ),
            ],
        };
        (None, lines, StateRole::Ok)
    }
}

fn absent() -> String {
    crate::caps::active_glyphs().emdash.into()
}
fn pct(value: Option<u8>) -> String {
    value
        .map(|value| format!("{value}%"))
        .unwrap_or_else(absent)
}
fn format_bytes(value: u64) -> String {
    kit::edge_fmt(NumFmt::default()).reading(Reading::Value(value as f64), Unit::Bytes, 10)
}
