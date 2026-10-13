//! The System ▸ Hosts section (hosts-as-resources): one row per `[host.*]`
//! config entry with a state glyph + reach + probed runtime, showing the
//! cursor host's details (target, image, arch, probe age, consent, inventory, last
//! error, and — deep — recent events). Reads `model.panel.hosts` (built
//! off-loop by hydration, live-merged by the loop's `HostRuntime` drain).
//! Action keys (p/r/c/x on the cursor host, m for the action menu, n to add a
//! host) are dispatched by the event loop via `handlers::host::section_key`;
//! the hint row mirrors them so they can't drift.

use thegn_core::theme::Hue;

use crate::host_ui::HostSnapshot;
use crate::seg::{Line, Seg, seg};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, f, fmt_secs, g, g2, hint_row, hue, list_detail,
    rule, t, wrap_text,
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
        let mut left = vec![state_glyph(h), seg(d(), format!(" {}", h.name))];
        left.push(seg(g(), format!("  {}", h.reach)));
        if !h.runtime.is_empty() {
            left.push(seg(g(), format!(" · {}", h.runtime)));
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
        ("n", "new"),
        ("p", "provision"),
        ("r", "probe"),
        ("m", "menu"),
        ("c", "grant"),
        ("x", "forget cache"),
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
                    seg(if selected { t() } else { d() }, format!(" {}", host.name)),
                    seg(g(), format!("  {} · {}", host.reach, host.runtime)),
                ],
                Some(i),
            )
        })
        .collect();
    let host = &hosts[cursor];
    let detail_w = ctx.cols.saturating_sub(list_w + 2);
    let mut detail: Vec<Vec<Seg>> = wrap_text(&format!("reach target  {}", host.target), detail_w)
        .into_iter()
        .map(|part| vec![seg(g2(), part)])
        .collect();
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
        ("n", "new"),
        ("p", "provision"),
        ("r", "probe"),
        ("m", "menu"),
        ("c", "grant"),
        ("x", "forget cache"),
    ]));
    rows
}

/// The cursor host's expanded detail block. Deep/full views add the recent
/// event trail; the compact view keeps to identity + freshness + failure.
fn detail_rows(h: &HostSnapshot, ctx: &SectionCtx) -> Vec<PanelRow> {
    let kv = |k: &str, v: String| {
        PanelRow::plain(Line::segs(vec![seg(g(), format!("  {k:<8}")), seg(f(), v)]))
    };
    let probe = match h.last_probe {
        Some(t) => {
            let ago = thegn_core::util::now().saturating_sub(t);
            if ago <= 0 {
                "just now".to_string()
            } else {
                format!("{} ago", fmt_secs(ago))
            }
        }
        None => "never".into(),
    };
    let mut rows = vec![kv("image", h.image.clone())];
    if !h.arch_os.is_empty() {
        rows.push(kv("arch", h.arch_os.clone()));
    }
    rows.push(kv("probe", probe));
    rows.push(kv("consent", h.consent.clone()));
    for inv in &h.inventory {
        rows.push(kv("have", inv.clone()));
    }
    if !h.error.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![
            seg(g(), "  "),
            seg(hue(Hue::Red), h.error.clone()),
        ])));
    }
    if ctx.deep() && !h.events.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g2(), "  EVENTS")])));
        for ev in h.events.iter().take(5) {
            rows.push(PanelRow::plain(Line::segs(vec![
                seg(g(), "   · "),
                seg(f(), ev.clone()),
            ])));
        }
    }
    rows
}
