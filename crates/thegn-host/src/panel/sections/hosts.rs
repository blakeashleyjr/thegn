//! The System ▸ Hosts section (hosts-as-resources): one row per `[host.*]`
//! config entry with a state glyph + reach + probed runtime, showing the
//! cursor host's details (target, image, arch, probe age, consent, inventory, last
//! error, and — deep — recent events). Reads `model.panel.hosts` (built
//! off-loop by hydration, live-merged by the loop's `HostRuntime` drain).
//! Action keys (p/r/c/x on the cursor host, m for the action menu, n to add a
//! host) are dispatched by the event loop via `handlers::host::section_key`;
//! the hint row mirrors them so they can't drift.

use thegn_core::pipeline_report::display_cell;
use thegn_core::theme::Hue;

use crate::host_ui::HostSnapshot;
use crate::seg::{Line, Seg, seg};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, f, fmt_secs, g, g2, hint_row, hue, list_detail,
    rule, t,
};

/// A host's hued state glyph (● ready / ◐ provisioning / ✗ failed / ○ new).
fn state_glyph(h: &HostSnapshot) -> Seg {
    match h.glyph() {
        "●" => seg(hue(Hue::Green), "●"),
        "◐" => seg(hue(Hue::Amber), "◐"),
        "✗" => seg(hue(Hue::Red), "✗"),
        _ => seg(g(), "○"),
    }
}

/// The section's closed-row summary: latest glyph + ready/failed/provisioning
/// counts ("— " when no hosts are configured).
pub(super) fn summary(model: &crate::chrome::FrameModel) -> Vec<Seg> {
    let hosts = &model.panel.hosts;
    if hosts.is_empty() {
        return vec![seg(g2(), "—")];
    }
    let ready = hosts.iter().filter(|h| h.glyph() == "●").count();
    let failed = hosts.iter().filter(|h| h.glyph() == "✗").count();
    let busy = hosts.iter().filter(|h| h.provisioning).count();
    let mut v = vec![seg(g(), format!("{}/{}", ready, hosts.len()))];
    if failed > 0 {
        v.push(seg(g(), " "));
        v.push(seg(hue(Hue::Red), format!("✗{failed}")));
    }
    if busy > 0 {
        v.push(seg(g(), " "));
        v.push(seg(hue(Hue::Amber), format!("◐{busy}")));
    }
    v
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let hosts = &ctx.model.panel.hosts;
    if hosts.is_empty() {
        let mut rows = Vec::new();
        if ctx.full() {
            rows.push(PanelRow::plain(Line::segs(vec![seg(d(), "HOSTS")])));
        }
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g(),
            "no hosts — n adds one (or add a [host.<name>] table)",
        )])));
        rows.push(hint_row(&[("n", "new host")]));
        return rows;
    }
    if ctx.full() {
        return full_view(ctx);
    }
    let mut rows: Vec<PanelRow> = Vec::new();
    // Each host row carries a `Row` hit (detail rows don't), so the enumerate
    // index lines up with `ui.cursor` and with `panel.hosts` — the action keys
    // (p/r/c/x, m) target `hosts[cursor]`.
    for (i, h) in hosts.iter().enumerate() {
        let mut left = vec![
            state_glyph(h),
            seg(d(), format!(" {}", display_cell(&h.name))),
        ];
        left.push(seg(g(), format!("  {}", display_cell(&h.reach))));
        if !h.runtime.is_empty() {
            left.push(seg(g(), format!(" · {}", display_cell(&h.runtime))));
        }
        rows.push(
            PanelRow::plain(Line::split(left, vec![seg(g(), h.short_status())]))
                .with_hit(PanelHit::Row(Section::Hosts, i)),
        );
        if i == ctx.ui.cursor {
            rows.extend(detail_rows(h, ctx));
        }
    }
    // Mirrors the statusbar strip (`section_keys::HOSTS`) — the two used to
    // advertise different key sets.
    rows.push(hint_row(&[
        ("n", "add host"),
        ("p", "provision now"),
        ("r", "re-probe"),
        ("m", "more"),
        ("c", "allow runtime install"),
        ("x", "forget cached state"),
    ]));
    rows
}

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let hosts = &ctx.model.panel.hosts;
    let list_w = 45_usize.min(ctx.cols / 2);
    let cursor = ctx.ui.cursor.min(hosts.len().saturating_sub(1));
    let list: Vec<(Vec<Seg>, Option<usize>)> = hosts
        .iter()
        .enumerate()
        .map(|(i, host)| {
            let selected = i == cursor;
            (
                vec![
                    seg(
                        if selected { t() } else { g() },
                        if selected { "▶ " } else { "  " },
                    ),
                    state_glyph(host),
                    seg(
                        if selected { t() } else { d() },
                        format!(" {}", display_cell(&host.name)),
                    ),
                    seg(
                        g(),
                        format!(
                            "  {} · {}",
                            display_cell(&host.reach),
                            display_cell(&host.runtime)
                        ),
                    ),
                ],
                Some(i),
            )
        })
        .collect();
    let host = &hosts[cursor];
    let mut detail: Vec<Vec<Seg>> = Vec::new();
    detail.extend(
        detail_rows(host, ctx)
            .into_iter()
            .filter_map(|row| match row.line {
                Line::Segs(segs) => Some(segs),
                Line::Blank => Some(Vec::new()),
                _ => None,
            }),
    );
    let mut rows = vec![PanelRow::plain(Line::segs(vec![seg(d(), "HOSTS")])), rule()];
    rows.extend(list_detail(
        Section::Hosts,
        &list,
        &detail,
        cursor,
        list_w,
        ctx.rows.saturating_sub(rows.len() + 1),
    ));
    rows.push(hint_row(&[
        ("n", "add host"),
        ("p", "provision now"),
        ("r", "re-probe"),
        ("m", "more"),
        ("c", "allow runtime install"),
        ("x", "forget cached state"),
    ]));
    rows
}

/// The cursor host's expanded detail block. Deep/full views add the recent
/// event trail; the compact view keeps to identity + freshness + failure.
fn detail_rows(h: &HostSnapshot, ctx: &SectionCtx) -> Vec<PanelRow> {
    let kv = |k: &str, v: String| {
        PanelRow::plain(Line::segs(vec![
            seg(g(), format!("  {k:<9}")),
            seg(f(), display_cell(&v)),
        ]))
    };
    let probe = match h.last_probe {
        Some(t) => {
            let ago = thegn_core::util::now().saturating_sub(t);
            if ago <= 0 {
                "last probed just now".to_string()
            } else {
                format!("last probed {} ago", fmt_secs(ago))
            }
        }
        None => "not probed yet".into(),
    };
    let address = if h.jump.is_empty() {
        h.target.clone()
    } else {
        format!("{} (via jump host {})", h.target, h.jump)
    };
    let system = match (h.arch_os.is_empty(), h.runtime.is_empty()) {
        (true, true) => "unknown until probed".to_string(),
        (false, true) => h.arch_os.clone(),
        (true, false) => h.runtime.clone(),
        (false, false) => format!("{} · {}", h.arch_os, h.runtime),
    };
    let mut uses: Vec<String> = Vec::new();
    if !h.envs.is_empty() {
        uses.push(format!("envs {}", h.envs.join(", ")));
    }
    if h.job_runner {
        uses.push("job runner".into());
    }
    let uses = if uses.is_empty() {
        "nothing yet (no [env.*] sets host = \"{name}\")".replace("{name}", &h.name)
    } else {
        uses.join(" · ")
    };
    let mut rows = vec![
        kv("address", address),
        kv("system", system),
        kv("reach", format!("{} — {probe}", h.reach)),
        kv("state", format!("{} — {}", h.short_status(), h.reason())),
        kv("next", h.next_action().to_string()),
        kv("used by", uses),
        kv("image", h.image.clone()),
        kv("consent", h.consent_plain()),
    ];
    for inv in &h.inventory {
        rows.push(kv("have", inv.clone()));
    }
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(g(), "  "),
        seg(g2(), "help: panel ▸ hosts · thegn host status"),
    ])));
    if ctx.deep() && !h.events.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g2(), "  EVENTS")])));
        for ev in h.events.iter().take(5) {
            rows.push(PanelRow::plain(Line::segs(vec![
                seg(g(), "   · "),
                seg(f(), display_cell(ev)),
            ])));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::content;
    use crate::chrome::FrameModel;
    use crate::host_ui::HostSnapshot;
    use crate::layout::PanelWidth;
    use crate::panel::sections::{Section, SectionCtx};

    fn render(hosts: Vec<HostSnapshot>) -> Vec<String> {
        let model = FrameModel {
            panel: crate::panel::PanelData {
                hosts,
                ..Default::default()
            },
            ..Default::default()
        };
        let ui = crate::panel::PanelUi {
            open: Section::Hosts,
            width: PanelWidth::Normal,
            ..Default::default()
        };
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 30,
        };
        content(&ctx)
            .iter()
            .map(|row| match &row.line {
                crate::seg::Line::Segs(s) => s.iter().map(|x| x.text.as_str()).collect::<String>(),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn unprovisioned_host_explains_itself() {
        let text = render(vec![HostSnapshot {
            name: "ws".into(),
            reach: "ssh".into(),
            target: "ws.example:2222".into(),
            jump: "bastion".into(),
            consent: "never (config)".into(),
            envs: vec!["dev".into()],
            job_runner: true,
            ..Default::default()
        }])
        .join("\n");
        for want in [
            "ws.example:2222 (via jump host bastion)",
            "unknown until probed",
            "not probed yet",
            "never probed or provisioned",
            "r probes the host, or p provisions it",
            "envs dev · job runner",
            "install_runtime = \"never\"",
            "help: panel ▸ hosts",
            "allow runtime install",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
    }

    #[test]
    fn ready_host_shows_system_and_no_action() {
        let text = render(vec![HostSnapshot {
            name: "b".into(),
            reach: "ssh".into(),
            target: "b".into(),
            state: "ready".into(),
            arch_os: "x86_64 linux".into(),
            runtime: "podman 5.0".into(),
            consent: "granted".into(),
            ..Default::default()
        }])
        .join("\n");
        assert!(text.contains("x86_64 linux · podman 5.0"), "{text}");
        assert!(text.contains("none needed"), "{text}");
        assert!(text.contains("nothing yet"), "{text}");
    }
}
