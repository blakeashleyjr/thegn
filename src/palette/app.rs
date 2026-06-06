//! The palette's root iocraft component: a prefix-routed input, a live nucleo
//! result list (streamed from `Core`), and a footer. Matching ticks on a 25ms
//! timer so streamed results (file walk / ripgrep) appear without blocking
//! input; the chosen action is recorded into `Shared` and enacted after exit.

use super::item::{self, Row};
use super::mode;
use super::preview;
use super::ui;
use super::Shared;
use iocraft::prelude::*;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Show the preview pane only when the terminal is wide enough to be useful.
const PREVIEW_MIN_WIDTH: u16 = 90;

#[component]
pub fn Palette(mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let (width, height) = hooks.use_terminal_size();
    let shared = hooks.use_context::<Shared>().clone();
    let mut query = hooks.use_state(String::new);
    let mut selected = hooks.use_state(|| 0usize);
    let mut redraw = hooks.use_state(|| 0u64);
    let mut should_exit = hooks.use_state(|| false);
    // Per-item action menu (Tab): when open, the list shows the selected row's
    // secondary actions instead of search results.
    let mut menu_open = hooks.use_state(|| false);
    // Memoized preview: (selection key, rendered lines). Rebuilt only when the
    // selected row's key changes, so streaming redraws never re-read files.
    let mut preview_cache = hooks.use_state(|| (String::new(), Vec::<String>::new()));

    // Poll loop: advance the matcher ~40fps, redrawing only when results change
    // (so streamed file/grep rows surface live without busy-looping the UI).
    {
        let core = shared.core.clone();
        hooks.use_future(async move {
            loop {
                smol::Timer::after(Duration::from_millis(25)).await;
                let changed = core.lock().map(|mut c| c.tick()).unwrap_or(false);
                if changed {
                    redraw.set(redraw.get().wrapping_add(1));
                }
            }
        });
    }

    // Keyboard: list navigation + activation. Text editing is owned by TextInput
    // (it has focus); we only claim the keys it ignores in single-line mode.
    {
        let current = shared.current.clone();
        let chosen = shared.chosen.clone();
        let total = shared.total.clone();
        let menu_row = shared.menu_row.clone();
        hooks.use_terminal_events(move |event| {
            let TerminalEvent::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = event
            else {
                return;
            };
            if kind == KeyEventKind::Release {
                return;
            }
            let ctrl = modifiers.contains(KeyModifiers::CONTROL);
            let max = total.load(Ordering::Relaxed).saturating_sub(1);

            let is_back = code == KeyCode::Esc || (ctrl && code == KeyCode::Char('c'));
            let is_up = code == KeyCode::Up || (ctrl && code == KeyCode::Char('p'));
            let is_down = code == KeyCode::Down || (ctrl && code == KeyCode::Char('n'));

            if is_back {
                // Back out of the action menu first; otherwise dismiss the palette.
                if menu_open.get() {
                    menu_open.set(false);
                    selected.set(0);
                } else {
                    should_exit.set(true);
                }
            } else if code == KeyCode::Tab {
                if menu_open.get() {
                    menu_open.set(false);
                    selected.set(0);
                } else if let Some(row) = current.lock().unwrap().get(selected.get()) {
                    *menu_row.lock().unwrap() = Some(row.clone());
                    menu_open.set(true);
                    selected.set(0);
                }
            } else if is_up {
                selected.set(selected.get().saturating_sub(1));
            } else if is_down {
                selected.set((selected.get() + 1).min(max));
            } else if code == KeyCode::Enter {
                if let Some(row) = current.lock().unwrap().get(selected.get()) {
                    *chosen.lock().unwrap() = Some(row.clone());
                }
                should_exit.set(true);
            }
        });
    }

    {
        // Called unconditionally to keep hook ordering stable; exits the render
        // loop on the next tick once a key handler has requested it.
        let mut system = hooks.use_context_mut::<SystemContext>();
        if should_exit.get() {
            system.exit();
        }
    }

    // --- derive view state ---
    let raw = query.read().clone();
    let parsed = mode::parse(&raw);
    let accent = parsed.mode.hue();

    // Chrome occupies a fixed number of rows (outer padding 2, bordered input 3,
    // footer 1, results top padding 1); the rest is the result viewport.
    let visible = (height as usize).saturating_sub(7).max(1);

    let show_preview = width >= PREVIEW_MIN_WIDTH;
    let preview_w: u16 = if show_preview {
        ((width as u32 * 9 / 20) as u16).clamp(32, 80)
    } else {
        0
    };

    let menu = menu_open.get();
    let (page, total, hit_cap, sel_row) = if menu {
        // Action menu: the selected row's secondary actions become the list.
        let base = shared.menu_row.lock().unwrap().clone();
        let rows = base.as_ref().map(item::secondary).unwrap_or_default();
        let total = rows.len();
        let sel = selected.get().min(total.saturating_sub(1));
        *shared.current.lock().unwrap() = rows.clone();
        shared.total.store(total, Ordering::Relaxed);
        let page: Vec<(Row, bool)> = rows
            .into_iter()
            .enumerate()
            .take(visible)
            .map(|(i, r)| (r, i == sel))
            .collect();
        // Keep previewing the underlying item while choosing an action.
        (page, total, false, base)
    } else {
        let mut c = shared.core.lock().unwrap();
        c.set_input(&parsed);
        let total = c.total();
        let sel = selected.get().min(total.saturating_sub(1));
        let offset = if sel >= visible { sel - visible + 1 } else { 0 };
        let rows = c.rows(offset + visible);
        // Mirror the ranked rows + count for the key handler.
        *shared.current.lock().unwrap() = rows.clone();
        shared.total.store(total, Ordering::Relaxed);
        let sel_row = rows.get(sel).cloned();
        let page: Vec<(Row, bool)> = rows
            .into_iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(i, r)| (r, i == sel))
            .collect();
        let hit_cap =
            parsed.mode == mode::Mode::Content && total >= super::sources::content::MAX_HITS;
        (page, total, hit_cap, sel_row)
    };

    // Rebuild the preview only when the selected row changes.
    let preview_lines: Vec<String> = if show_preview {
        match &sel_row {
            Some(row) => {
                let k = preview::key(row);
                if preview_cache.read().0 != k {
                    let lines = preview::build(row);
                    let mut w = preview_cache.write();
                    w.0 = k;
                    w.1 = lines.clone();
                    lines
                } else {
                    preview_cache.read().1.clone()
                }
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    // Clip each preview line to the pane width (NoWrap + manual clip avoids any
    // layout blow-up from long lines).
    let clip_w = preview_w.saturating_sub(3) as usize;
    let preview_lines: Vec<String> = preview_lines
        .into_iter()
        .take(visible)
        .map(|l| clip(&l, clip_w))
        .collect();

    let tint = ui::accent_tint(accent);
    let count_label = if hit_cap {
        format!("{}+", super::sources::content::MAX_HITS)
    } else {
        total.to_string()
    };

    element! {
        View(
            width: width,
            height: height,
            background_color: ui::bg0(),
            flex_direction: FlexDirection::Column,
            padding: 1,
        ) {
            // --- input row ---
            View(
                border_style: BorderStyle::Round,
                border_color: ui::color(accent),
                background_color: ui::bg1(),
                padding_left: 1,
                padding_right: 1,
                flex_direction: FlexDirection::Row,
                gap: 1,
            ) {
                Text(content: "❯", color: ui::color(accent), weight: Weight::Bold)
                View(flex_grow: 1.0) {
                    TextInput(
                        has_focus: true,
                        color: ui::text(),
                        value: raw.clone(),
                        on_change: move |v| {
                            query.set(v);
                            selected.set(0);
                        },
                    )
                }
                View(background_color: tint, padding_left: 1, padding_right: 1) {
                    Text(
                        content: if menu { "ACTIONS" } else { parsed.mode.label() },
                        color: ui::color(accent),
                        weight: Weight::Bold,
                    )
                }
            }

            // --- results (list + optional preview) ---
            View(flex_grow: 1.0, flex_direction: FlexDirection::Row, padding_top: 1, gap: 1) {
                View(flex_grow: 1.0, flex_direction: FlexDirection::Column) {
                    #(page.into_iter().map(|(row, sel)| {
                        let row_bg: Option<Color> = if sel { Some(tint) } else { None };
                        let label_weight = if sel { Weight::Bold } else { Weight::Normal };
                        element! {
                            View(
                                flex_direction: FlexDirection::Row,
                                padding_left: 1,
                                padding_right: 1,
                                gap: 1,
                                background_color: row_bg,
                            ) {
                                Text(content: row.glyph, color: ui::color(row.hue))
                                Text(content: row.label, color: ui::text(), weight: label_weight, wrap: TextWrap::NoWrap)
                                View(flex_grow: 1.0)
                                Text(content: row.detail, color: ui::faint(), wrap: TextWrap::NoWrap)
                            }
                        }
                    }))
                }
                #(show_preview.then(|| element! {
                    View(
                        width: preview_w,
                        flex_direction: FlexDirection::Column,
                        border_style: BorderStyle::Round,
                        border_color: ui::border(),
                        padding_left: 1,
                        padding_right: 1,
                    ) {
                        #(preview_lines.into_iter().map(|line| element! {
                            Text(content: line, color: ui::dim(), wrap: TextWrap::NoWrap)
                        }))
                    }
                }))
            }

            // --- footer ---
            View(flex_direction: FlexDirection::Row, gap: 2) {
                Text(content: format!("{} ▸", parsed.mode.label()), color: ui::color(accent))
                Text(content: format!("{count_label} results"), color: ui::faint())
                View(flex_grow: 1.0)
                Text(content: "↑↓ move", color: ui::faint())
                Text(content: "↵ run", color: ui::faint())
                Text(content: if menu { "⇥ back" } else { "⇥ actions" }, color: ui::faint())
                Text(content: "esc close", color: ui::faint())
            }
        }
    }
}

/// Clip a string to at most `max` characters (preview lines are rendered NoWrap;
/// this bounds them to the pane so nothing overflows the layout).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}
