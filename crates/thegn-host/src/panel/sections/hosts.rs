//! The System ▸ Hosts section (hosts-as-resources): one row per `[host.*]`
//! config entry with a state glyph + reach + probed runtime, showing the
//! cursor host's details (address and jump host, system, reach and probe age,
//! state with a reason and next key, what uses it, image, plain-words consent,
//! inventory, and — deep — recent events). Reads `model.panel.hosts` (built
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
    rule, t, wrap_text,
};

/// Key column width of a detail row (`"  "` indent + 9-wide key).
const KEY_W: usize = 11;

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
            rows.extend(detail_rows(h, ctx, ctx.cols));
        }
    }
    // Mirrors the statusbar strip (`section_keys::HOSTS`) — the two used to
    // advertise different key sets.
    rows.push(hint_row(&[
        ("n", "add host"),
        ("p", "provision"),
        ("r", "re-probe"),
        ("m", "more"),
        ("c", "allow install"),
        ("x", "forget state"),
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
    let detail_w = ctx.cols.saturating_sub(list_w + 2);
    let detail: Vec<Vec<Seg>> = detail_rows(host, ctx, detail_w)
        .into_iter()
        .filter_map(|row| match row.line {
            Line::Segs(segs) => Some(segs),
            Line::Blank => Some(Vec::new()),
            _ => None,
        })
        .collect();
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
        ("p", "provision"),
        ("r", "re-probe"),
        ("m", "more"),
        ("c", "allow install"),
        ("x", "forget state"),
    ]));
    rows
}

/// The cursor host's expanded detail block, `width` columns wide: what it is,
/// reachability, state with a reason and the next key, what uses it, image,
/// consent and inventory. Values wrap under the key column so the plain-words
/// lines are never clipped. Deep/full views add the recent event trail.
fn detail_rows(h: &HostSnapshot, ctx: &SectionCtx, width: usize) -> Vec<PanelRow> {
    let val_w = width.saturating_sub(KEY_W);
    let mut rows: Vec<PanelRow> = Vec::new();
    let mut kv = |k: &str, v: String| {
        for (i, part) in wrap_text(&display_cell(&v), val_w).into_iter().enumerate() {
            let key = if i == 0 {
                format!("  {k:<9}")
            } else {
                " ".repeat(KEY_W)
            };
            rows.push(PanelRow::plain(Line::segs(vec![
                seg(g(), key),
                seg(f(), part),
            ])));
        }
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
    kv("address", address);
    kv("system", system);
    kv("reach", format!("{} — {probe}", h.reach));
    kv("state", format!("{} — {}", h.short_status(), h.reason()));
    kv("next", h.next_action().to_string());
    kv("used by", uses);
    kv("image", h.image.clone());
    kv("consent", h.consent_plain());
    for inv in &h.inventory {
        kv("have", inv.clone());
    }
    kv(
        "help",
        "thegn help panel (hosts) · thegn host status".into(),
    );
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
        render_at(hosts, 80)
    }

    fn render_at(hosts: Vec<HostSnapshot>, cols: usize) -> Vec<String> {
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
            cols,
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
            "thegn help panel",
            "allow install",
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

    #[test]
    fn narrow_panel_wraps_detail_values_instead_of_clipping() {
        let cols = 40;
        let lines = render_at(
            vec![HostSnapshot {
                name: "ws-mac-1".into(),
                reach: "ssh".into(),
                target: "ws-mac-1".into(),
                consent: "never (config)".into(),
                ..Default::default()
            }],
            cols,
        );
        let detail: Vec<&String> = lines.iter().filter(|l| l.starts_with("  ")).collect();
        assert!(detail.len() > 8, "{lines:#?}");
        for line in &detail {
            assert!(line.chars().count() <= cols, "over {cols} cols: {line:?}");
        }
        // The consent explanation spans continuation rows under the key column.
        let consent = detail.iter().position(|l| l.contains("consent")).unwrap();
        assert!(
            detail[consent + 1].starts_with(&" ".repeat(super::KEY_W)),
            "{lines:#?}"
        );
    }
}
