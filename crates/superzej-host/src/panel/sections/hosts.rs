//! The System ▸ Hosts section (hosts-as-resources): one row per `[host.*]`
//! config entry with a state glyph + reach + probed runtime, expanding the
//! cursor host's details (image, arch, probe age, consent, inventory, last
//! error, and — deep — recent events). Reads `model.panel.hosts` (built
//! off-loop by hydration, live-merged by the loop's `HostRuntime` drain).
//! Action keys (p/r/c/x) are dispatched by the event loop via
//! `host_ui::panel_key`; the hint row mirrors them so they can't drift.

use superzej_core::theme::Hue;

use crate::host_ui::HostSnapshot;
use crate::seg::{Line, Seg, seg};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, f, fmt_secs, g, g2, hint_row, hue};

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
            "no hosts configured — add a [host.<name>] table",
        )])));
        return rows;
    }
    let mut rows: Vec<PanelRow> = Vec::new();
    if ctx.full() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(d(), "HOSTS")])));
    }
    // Each host row carries a `Row` hit (detail rows don't), so the enumerate
    // index lines up with `ui.cursor` and with `panel.hosts` — the action keys
    // (p/r/c/x) target `hosts[cursor]`.
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
    rows.push(hint_row(&[
        ("p", "provision"),
        ("r", "re-probe"),
        ("c", "grant install"),
        ("x", "rm-cache"),
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
            let ago = superzej_core::util::now().saturating_sub(t);
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
