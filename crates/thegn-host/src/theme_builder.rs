//! Pure state, reducer, and renderer for the theme-builder modal.

use std::path::PathBuf;

use termwiz::input::{KeyCode, Modifiers, MouseButtons, MouseEvent};
use termwiz::surface::Surface;

use thegn_core::theme::{Hue, Palette};
use thegn_core::theme_user::UserTheme;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, box_rect, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};

const CATALOG_COLS: usize = 27;
const TOKEN_ROWS: usize = 20;
const ACTION_ROW: usize = TOKEN_ROWS + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorRole {
    Bg0,
    Bg1,
    Panel,
    Panel2,
    Raise,
    Border,
    Text,
    Dim,
    Faint,
    Ghost,
    Accent,
    Focus,
    Teal,
    Magenta,
    Purple,
    Green,
    Amber,
    Red,
    Blue,
    Orange,
}

const ROLES: [(ColorRole, &str); TOKEN_ROWS] = [
    (ColorRole::Bg0, "bg0"),
    (ColorRole::Bg1, "bg1"),
    (ColorRole::Panel, "panel"),
    (ColorRole::Panel2, "panel2"),
    (ColorRole::Raise, "raise"),
    (ColorRole::Border, "border"),
    (ColorRole::Text, "text"),
    (ColorRole::Dim, "dim"),
    (ColorRole::Faint, "faint"),
    (ColorRole::Ghost, "ghost"),
    (ColorRole::Accent, "accent"),
    (ColorRole::Focus, "focus"),
    (ColorRole::Teal, "teal"),
    (ColorRole::Magenta, "magenta"),
    (ColorRole::Purple, "purple"),
    (ColorRole::Green, "green"),
    (ColorRole::Amber, "amber"),
    (ColorRole::Red, "red"),
    (ColorRole::Blue, "blue"),
    (ColorRole::Orange, "orange"),
];

#[derive(Debug, Clone)]
pub(crate) struct CatalogItem {
    pub name: String,
    pub theme: UserTheme,
    pub builtin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuilderEvent {
    None,
    Close,
    Import(PathBuf),
    Save(UserTheme),
    Apply { preset: String, theme: UserTheme },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    SaveName,
    ImportPath,
}

/// The builder owns only transient draft state. The live palette is installed
/// by the handler after each reducer mutation, keeping this module render-pure.
pub(crate) struct ThemeBuilder {
    snapshot: Palette,
    candidate: Palette,
    draft: UserTheme,
    catalog: Vec<CatalogItem>,
    selected: usize,
    focus: usize,
    editing: Option<(ColorRole, String, usize)>,
    input: Option<(InputMode, String)>,
    status: Option<String>,
    pending: bool,
}

impl ThemeBuilder {
    pub(crate) fn open(cfg: &thegn_core::config::Config, user_themes: &[UserTheme]) -> Self {
        let catalog = catalog(cfg, user_themes);
        let selected = catalog
            .iter()
            .position(|item| item.name == cfg.theme.preset)
            .unwrap_or(0);
        let draft = catalog
            .get(selected)
            .map(|item| item.theme.clone())
            .unwrap_or_else(|| UserTheme::from_palette(&cfg.theme.preset, &cfg.palette()));
        let candidate = draft.palette().unwrap_or_else(|_| cfg.palette());
        Self {
            snapshot: cfg.palette(),
            candidate,
            draft,
            catalog,
            selected,
            focus: 0,
            editing: None,
            input: None,
            status: None,
            pending: false,
        }
    }

    pub(crate) fn candidate(&self) -> &Palette {
        &self.candidate
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.candidate != &self.snapshot
    }

    pub(crate) fn active(&self) -> bool {
        true
    }

    pub(crate) fn set_catalog(&mut self, cfg: &thegn_core::config::Config, users: &[UserTheme]) {
        let current_name = self.draft.meta.name.clone();
        self.catalog = catalog(cfg, users);
        if let Some(index) = self
            .catalog
            .iter()
            .position(|item| item.name == current_name)
        {
            self.selected = index;
        }
    }

    pub(crate) fn config_reloaded(&mut self, cfg: &thegn_core::config::Config) {
        self.snapshot = cfg.palette();
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> bool {
        let Some((_, value)) = self.input.as_mut() else {
            return false;
        };
        *value = sanitize_field(text);
        self.status = None;
        true
    }

    pub(crate) fn handle_mouse(
        &mut self,
        event: &MouseEvent,
        mx: usize,
        my: usize,
        screen: Rect,
        dismiss_outside: bool,
    ) -> BuilderEvent {
        let spec = layer_spec();
        let Some(outer) = box_rect(&spec, screen) else {
            return BuilderEvent::None;
        };
        let left = event.mouse_buttons.contains(MouseButtons::LEFT);
        let press = left && !event.mouse_buttons.contains(MouseButtons::MOTION);
        if !outer.contains(mx, my) {
            if press && dismiss_outside && !self.is_dirty() && self.input.is_none() {
                return BuilderEvent::Close;
            }
            return BuilderEvent::None;
        }
        if !press {
            return BuilderEvent::None;
        }
        let inner = Rect {
            x: outer.x + 2,
            y: outer.y + 1,
            cols: outer.cols.saturating_sub(4),
            rows: outer.rows.saturating_sub(2),
        };
        if mx < inner.x + CATALOG_COLS {
            let index = my.saturating_sub(inner.y + 1);
            if index < self.catalog.len() {
                self.select(index);
            }
        } else {
            let row = my.saturating_sub(inner.y + 1);
            if row < TOKEN_ROWS {
                self.focus = row;
                self.editing = None;
            } else if row == ACTION_ROW {
                return self.apply_event();
            }
        }
        BuilderEvent::None
    }

    pub(crate) fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> BuilderEvent {
        if let Some((mode, value)) = &mut self.input {
            match key {
                KeyCode::Escape => {
                    self.input = None;
                    self.status = None;
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Enter => {
                    let value = sanitize_field(value);
                    if value.is_empty() {
                        self.status = Some(match mode {
                            InputMode::SaveName => "Save as needs a name".into(),
                            InputMode::ImportPath => "Import needs a local path".into(),
                        });
                    } else {
                        match mode {
                            InputMode::SaveName => {
                                self.draft.meta.name = value;
                                self.candidate = self
                                    .draft
                                    .palette()
                                    .unwrap_or_else(|_| self.candidate.clone());
                                self.input = None;
                                self.pending = true;
                                return BuilderEvent::Save(self.draft.clone());
                            }
                            InputMode::ImportPath => {
                                self.input = None;
                                self.pending = true;
                                return BuilderEvent::Import(PathBuf::from(value));
                            }
                        }
                    }
                }
                KeyCode::Char(c) => {
                    if !c.is_control() {
                        value.push(*c);
                    }
                }
                _ => {}
            }
            return BuilderEvent::None;
        }

        if let Some((role, original, cursor)) = &mut self.editing {
            match key {
                KeyCode::Escape => {
                    set_role(&mut self.draft, *role, original.clone());
                    self.refresh_candidate();
                    self.editing = None;
                }
                KeyCode::LeftArrow => *cursor = cursor.saturating_sub(1),
                KeyCode::RightArrow => {
                    *cursor = (*cursor + 1).min(role_value(&self.draft, *role).len())
                }
                KeyCode::Backspace => {
                    let mut value = role_value(&self.draft, *role);
                    if *cursor > 0 {
                        value.remove(*cursor - 1);
                        *cursor -= 1;
                        set_role(&mut self.draft, *role, value);
                        self.refresh_candidate();
                    }
                }
                KeyCode::Enter => {
                    if self.draft.validate().is_ok() {
                        self.editing = None;
                    } else {
                        self.status = Some("Use #rgb or #rrggbb".into());
                    }
                }
                KeyCode::Char(c) if !c.is_control() => {
                    let mut value = role_value(&self.draft, *role);
                    value.insert(*cursor, *c);
                    *cursor += 1;
                    set_role(&mut self.draft, *role, value);
                    self.refresh_candidate();
                }
                _ => {}
            }
            return BuilderEvent::None;
        }

        if mods.contains(Modifiers::CTRL) && matches!(key, KeyCode::Char('s' | 'S')) {
            self.input = Some((InputMode::SaveName, self.draft.meta.name.clone()));
            self.status = None;
            return BuilderEvent::None;
        }
        match key {
            KeyCode::Escape => BuilderEvent::Close,
            KeyCode::Char('i') if mods.is_empty() => {
                self.input = Some((InputMode::ImportPath, String::new()));
                self.status = None;
                BuilderEvent::None
            }
            KeyCode::UpArrow => {
                if !self.catalog.is_empty() {
                    self.select(self.selected.saturating_sub(1));
                }
                BuilderEvent::None
            }
            KeyCode::DownArrow => {
                if !self.catalog.is_empty() {
                    self.select((self.selected + 1).min(self.catalog.len() - 1));
                }
                BuilderEvent::None
            }
            KeyCode::Tab => {
                if mods.contains(Modifiers::SHIFT) {
                    self.focus = if self.focus == 0 {
                        ACTION_ROW
                    } else {
                        self.focus - 1
                    };
                } else {
                    self.focus = (self.focus + 1).min(ACTION_ROW);
                }
                BuilderEvent::None
            }
            KeyCode::Enter if self.focus == ACTION_ROW => self.apply_event(),
            KeyCode::Enter => {
                let (role, _) = ROLES[self.focus.min(TOKEN_ROWS - 1)];
                let value = role_value(&self.draft, role);
                self.editing = Some((role, value.clone(), value.len()));
                BuilderEvent::None
            }
            _ => BuilderEvent::None,
        }
    }

    fn apply_event(&mut self) -> BuilderEvent {
        if self.pending {
            self.status = Some("Waiting for theme store…".into());
            return BuilderEvent::None;
        }
        if let Err(error) = self.draft.validate() {
            self.status = Some(error.to_string());
            return BuilderEvent::None;
        }
        self.pending = true;
        BuilderEvent::Apply {
            preset: self
                .catalog
                .get(self.selected)
                .map(|item| item.name.clone())
                .unwrap_or_else(|| self.draft.meta.name.clone()),
            theme: self.draft.clone(),
        }
    }

    fn select(&mut self, index: usize) {
        let Some(item) = self.catalog.get(index) else {
            return;
        };
        self.selected = index;
        self.draft = item.theme.clone();
        self.refresh_candidate();
        self.status = None;
    }

    fn refresh_candidate(&mut self) {
        match self.draft.palette() {
            Ok(palette) => {
                self.candidate = palette;
                self.status = None;
            }
            Err(error) => self.status = Some(error.to_string()),
        }
    }

    pub(crate) fn store_completed(&mut self, result: Result<UserTheme, String>) {
        self.pending = false;
        match result {
            Ok(theme) => {
                self.draft = theme;
                self.refresh_candidate();
                self.status = None;
            }
            Err(error) => self.status = Some(error),
        }
    }

    pub(crate) fn import_completed(&mut self, result: Result<UserTheme, String>) {
        self.pending = false;
        match result {
            Ok(theme) => {
                self.draft = theme;
                self.refresh_candidate();
                self.status = Some("Imported preview — Ctrl+S to save it".into());
            }
            Err(error) => self.status = Some(error),
        }
    }

    pub(crate) fn cancel_palette(&self) -> Palette {
        self.snapshot.clone()
    }

    pub(crate) fn set_status(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
    }

    pub(crate) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub(crate) fn input_value(&self) -> Option<&str> {
        self.input.as_ref().map(|(_, value)| value.as_str())
    }
}

fn catalog(cfg: &thegn_core::config::Config, users: &[UserTheme]) -> Vec<CatalogItem> {
    let mut out = thegn_core::theme::PRESETS
        .iter()
        .map(|name| CatalogItem {
            name: (*name).into(),
            theme: UserTheme::from_palette(name, &cfg.palette_with_preset(name)),
            builtin: true,
        })
        .collect::<Vec<_>>();
    for user in users {
        if !out.iter().any(|item| item.name == user.meta.name) {
            out.push(CatalogItem {
                name: user.meta.name.clone(),
                theme: user.clone(),
                builtin: false,
            });
        }
    }
    out
}

fn role_value(theme: &UserTheme, role: ColorRole) -> String {
    match role {
        ColorRole::Bg0 => theme.colors.bg0.clone(),
        ColorRole::Bg1 => theme.colors.bg1.clone(),
        ColorRole::Panel => theme.colors.panel.clone(),
        ColorRole::Panel2 => theme.colors.panel2.clone(),
        ColorRole::Raise => theme.colors.raise.clone(),
        ColorRole::Border => theme.colors.border.clone(),
        ColorRole::Text => theme.colors.text.clone(),
        ColorRole::Dim => theme.colors.dim.clone(),
        ColorRole::Faint => theme.colors.faint.clone(),
        ColorRole::Ghost => theme.colors.ghost.clone(),
        ColorRole::Accent => theme.colors.accent.clone(),
        ColorRole::Focus => theme.colors.focus.clone(),
        ColorRole::Teal => theme.hues.teal.clone(),
        ColorRole::Magenta => theme.hues.magenta.clone(),
        ColorRole::Purple => theme.hues.purple.clone(),
        ColorRole::Green => theme.hues.green.clone(),
        ColorRole::Amber => theme.hues.amber.clone(),
        ColorRole::Red => theme.hues.red.clone(),
        ColorRole::Blue => theme.hues.blue.clone(),
        ColorRole::Orange => theme.hues.orange.clone(),
    }
}

fn set_role(theme: &mut UserTheme, role: ColorRole, value: String) {
    match role {
        ColorRole::Bg0 => theme.colors.bg0 = value,
        ColorRole::Bg1 => theme.colors.bg1 = value,
        ColorRole::Panel => theme.colors.panel = value,
        ColorRole::Panel2 => theme.colors.panel2 = value,
        ColorRole::Raise => theme.colors.raise = value,
        ColorRole::Border => theme.colors.border = value,
        ColorRole::Text => theme.colors.text = value,
        ColorRole::Dim => theme.colors.dim = value,
        ColorRole::Faint => theme.colors.faint = value,
        ColorRole::Ghost => theme.colors.ghost = value,
        ColorRole::Accent => theme.colors.accent = value,
        ColorRole::Focus => theme.colors.focus = value,
        ColorRole::Teal => theme.hues.teal = value,
        ColorRole::Magenta => theme.hues.magenta = value,
        ColorRole::Purple => theme.hues.purple = value,
        ColorRole::Green => theme.hues.green = value,
        ColorRole::Amber => theme.hues.amber = value,
        ColorRole::Red => theme.hues.red = value,
        ColorRole::Blue => theme.hues.blue = value,
        ColorRole::Orange => theme.hues.orange = value,
    }
}

fn role_token(role: ColorRole) -> Tok {
    match role {
        ColorRole::Bg0 => Tok::Slot(S::Bg0),
        ColorRole::Bg1 => Tok::Slot(S::Bg1),
        ColorRole::Panel => Tok::Slot(S::Panel),
        ColorRole::Panel2 => Tok::Slot(S::Panel2),
        ColorRole::Raise => Tok::Slot(S::Raise),
        ColorRole::Border => Tok::Slot(S::Border),
        ColorRole::Text => Tok::Slot(S::Text),
        ColorRole::Dim => Tok::Slot(S::Dim),
        ColorRole::Faint => Tok::Slot(S::Faint),
        ColorRole::Ghost => Tok::Slot(S::Ghost),
        ColorRole::Accent => Tok::Slot(S::Accent),
        ColorRole::Focus => Tok::Slot(S::Focus),
        ColorRole::Teal => Tok::Hue(Hue::Teal),
        ColorRole::Magenta => Tok::Hue(Hue::Magenta),
        ColorRole::Purple => Tok::Hue(Hue::Purple),
        ColorRole::Green => Tok::Hue(Hue::Green),
        ColorRole::Amber => Tok::Hue(Hue::Amber),
        ColorRole::Red => Tok::Hue(Hue::Red),
        ColorRole::Blue => Tok::Hue(Hue::Blue),
        ColorRole::Orange => Tok::Hue(Hue::Orange),
    }
}

fn sanitize_field(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .into()
}

fn layer_spec() -> LayerSpec {
    LayerSpec {
        title: "theme builder".into(),
        badge: Some(" Ctrl+Alt+Shift+t ".into()),
        cols: 106,
        rows: ACTION_ROW + 4,
        anchor: Anchor::Center,
        bg: Tok::Slot(S::Panel),
        border: Tok::Slot(S::Focus),
        ..LayerSpec::default()
    }
}

impl ThemeBuilder {
    pub(crate) fn render(&self, surface: &mut Surface, screen: Rect) {
        let Some(inner) = open_layer(surface, screen, &layer_spec()) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(vec![
                seg(Tok::Slot(S::Accent), "catalog").bold(),
                sp(2),
                seg(Tok::Slot(S::Dim), "editable palette + live preview"),
            ]),
            panel,
        );
        for (row, item) in self.catalog.iter().enumerate() {
            let y = inner.y + 1 + row;
            if y >= inner.y + inner.rows {
                break;
            }
            let selected = row == self.selected;
            let bg = if selected { Tok::SelAccent } else { panel };
            let fg = if selected {
                Tok::Slot(S::Text)
            } else {
                Tok::Slot(S::Dim)
            };
            let marker = if selected {
                crate::caps::glyph(crate::caps::Glyph::ArrowRight)
            } else {
                " "
            };
            let kind = if item.builtin { "builtin" } else { "user" };
            seg::draw_line(
                surface,
                inner.x,
                y,
                CATALOG_COLS.min(inner.cols),
                &Line::segs(vec![
                    seg(fg, format!("{marker} {}", item.name)),
                    sp(1),
                    seg(Tok::Slot(S::Ghost), kind),
                ]),
                bg,
            );
        }
        for (row, (role, name)) in ROLES.iter().enumerate() {
            let y = inner.y + 1 + row;
            if y >= inner.y + inner.rows {
                break;
            }
            let focused = self.focus == row;
            let bg = if focused {
                Tok::Sel(Hue::Blue, 28)
            } else {
                panel
            };
            let value = role_value(&self.draft, *role);
            let fg = if focused {
                Tok::Slot(S::Text)
            } else {
                role_token(*role)
            };
            let prefix = if focused { "› " } else { "  " };
            seg::draw_line(
                surface,
                inner.x + CATALOG_COLS + 2,
                y,
                inner.cols.saturating_sub(CATALOG_COLS + 2),
                &Line::segs(vec![
                    seg(fg, format!("{prefix}{name:<8}")),
                    seg(role_token(*role), value),
                ]),
                bg,
            );
        }
        let action_y = inner.y + 1 + ACTION_ROW;
        if action_y < inner.y + inner.rows {
            let bg = if self.focus == ACTION_ROW {
                Tok::SelAccent
            } else {
                panel
            };
            seg::draw_line(
                surface,
                inner.x + CATALOG_COLS + 2,
                action_y,
                inner.cols.saturating_sub(CATALOG_COLS + 2),
                &Line::segs(vec![
                    Seg::key(" Apply ".into()),
                    sp(1),
                    seg(Tok::Slot(S::Text), "Enter"),
                    sp(2),
                    seg(Tok::Slot(S::Dim), "Ctrl+S save as · i import"),
                ]),
                bg,
            );
        }
        render_preview(surface, inner, panel);
        if let Some((mode, value)) = &self.input {
            let label = match mode {
                InputMode::SaveName => "save as name",
                InputMode::ImportPath => "import local path",
            };
            let y = inner.y + inner.rows.saturating_sub(2);
            if y < inner.y + inner.rows {
                seg::draw_line(
                    surface,
                    inner.x,
                    y,
                    inner.cols,
                    &Line::segs(vec![
                        seg(Tok::Slot(S::Accent), format!("{label}: ")).bold(),
                        seg(Tok::Slot(S::Text), value.to_string()).into_caret(),
                    ]),
                    Tok::Slot(S::Raise),
                );
            }
        } else if let Some(status) = &self.status {
            let y = inner.y + inner.rows.saturating_sub(1);
            if y < inner.y + inner.rows {
                seg::draw_line(
                    surface,
                    inner.x,
                    y,
                    inner.cols,
                    &Line::segs(vec![seg(Tok::Hue(Hue::Red), status.clone())]),
                    panel,
                );
            }
        }
    }
}

fn render_preview(surface: &mut Surface, inner: Rect, panel: Tok) {
    let x = inner.x;
    let y = inner.y + TOKEN_ROWS + 2;
    if y >= inner.y + inner.rows {
        return;
    }
    let dot = crate::caps::glyph(crate::caps::Glyph::DotFilled);
    let rows = [
        Line::segs(vec![
            seg(Tok::Slot(S::Dim), format!("{dot} ")),
            seg(Tok::Slot(S::Text), "sidebar row"),
            sp(1),
            seg(Tok::Hue(Hue::Teal), "active"),
        ]),
        Line::segs(vec![
            seg(Tok::Slot(S::Accent), " main "),
            sp(1),
            seg(Tok::Slot(S::Focus), "focused tab"),
            sp(1),
            seg(Tok::Hue(Hue::Purple), "●"),
        ]),
        Line::segs(vec![
            seg(Tok::Slot(S::ActivityActive), " status "),
            sp(1),
            seg(Tok::Hue(Hue::Green), "ready"),
            sp(1),
            seg(Tok::Hue(Hue::Amber), "warning"),
            sp(1),
            seg(Tok::Hue(Hue::Red), "error"),
        ]),
        Line::segs(vec![
            seg(Tok::Hue(Hue::Green), "+ added hunk"),
            sp(1),
            seg(Tok::Hue(Hue::Red), "- removed hunk"),
            sp(1),
            seg(Tok::Sel(Hue::Blue, 35), "diff"),
        ]),
        Line::segs(vec![
            seg(Tok::Slot(S::Border), " pane "),
            sp(1),
            seg(Tok::Slot(S::Text), "structural text"),
            sp(1),
            seg(Tok::Slot(S::Ghost3), "···"),
        ]),
        Line::segs(vec![
            seg(Tok::SelAccent, " selected row "),
            sp(1),
            seg(Tok::Heat(4), "heat"),
            sp(1),
            seg(Tok::Hue(Hue::Orange), "activity"),
        ]),
    ];
    for (row, line) in rows.iter().enumerate() {
        if y + row >= inner.y + inner.rows {
            break;
        }
        seg::draw_line(surface, x, y + row, inner.cols.min(48), line, panel);
    }
}
