//! Pure state, reducer, and renderer for the theme-builder modal.

use std::path::PathBuf;

use termwiz::input::{KeyCode, Modifiers, MouseButtons, MouseEvent};
use termwiz::surface::Surface;

use thegn_core::theme::{Hue, Palette};
use thegn_core::theme_contrast::{self, Bar};
use thegn_core::theme_user::UserTheme;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, box_rect, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use crate::theme_store::ThemeOverrides;

const CATALOG_COLS: usize = 27;
const TOKEN_ROWS: usize = 20;
const ACTION_ROW: usize = TOKEN_ROWS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    Apply {
        preset: String,
        theme: UserTheme,
        overrides: Option<Box<ThemeOverrides>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    SaveName,
    ImportPath,
}

/// The builder owns only transient draft state. The live palette is installed
/// by the handler after each reducer mutation, keeping this module render-pure.
pub(crate) struct ThemeBuilder {
    config: thegn_core::config::Config,
    users: Vec<UserTheme>,
    active_name: String,
    snapshot: Palette,
    candidate: Palette,
    draft: UserTheme,
    catalog: Vec<CatalogItem>,
    selected: usize,
    focus: usize,
    editing: Option<(ColorRole, String, usize, bool)>,
    input: Option<(InputMode, String)>,
    status: Option<String>,
    pending: bool,
    requires_save: bool,
    edited: Vec<ColorRole>,
}

impl ThemeBuilder {
    pub(crate) fn open(cfg: &thegn_core::config::Config, user_themes: &[UserTheme]) -> Self {
        let catalog = catalog(cfg, user_themes);
        let selected = catalog
            .iter()
            .position(|item| item.name == cfg.theme.preset)
            .unwrap_or(0);
        let candidate = cfg.palette_with_user_themes(&cfg.theme.preset, user_themes);
        let draft = UserTheme::from_palette(&cfg.theme.preset, &candidate);
        Self {
            config: cfg.clone(),
            users: user_themes.to_vec(),
            active_name: cfg.theme.preset.clone(),
            snapshot: cfg.palette_with_user_themes(&cfg.theme.preset, user_themes),
            candidate,
            draft,
            catalog,
            selected,
            focus: 0,
            editing: None,
            input: None,
            status: None,
            pending: false,
            requires_save: false,
            edited: Vec::new(),
        }
    }

    pub(crate) fn candidate(&self) -> &Palette {
        &self.candidate
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.candidate != self.snapshot
    }

    pub(crate) fn set_catalog(&mut self, cfg: &thegn_core::config::Config, users: &[UserTheme]) {
        self.config = cfg.clone();
        self.users = users.to_vec();
        self.catalog = catalog(cfg, users);
        if let Some(index) = self
            .catalog
            .iter()
            .position(|item| item.name == self.active_name)
        {
            self.selected = index;
        }
        self.refresh_candidate();
    }

    pub(crate) fn config_reloaded(&mut self, cfg: &thegn_core::config::Config) {
        self.config = cfg.clone();
        self.snapshot = cfg.palette_with_user_themes(&cfg.theme.preset, &self.users);
        self.refresh_candidate();
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
        mouse_left_down: &mut bool,
    ) -> BuilderEvent {
        let spec = layer_spec();
        let Some(outer) = box_rect(&spec, screen) else {
            return BuilderEvent::None;
        };
        let left = event.mouse_buttons.contains(MouseButtons::LEFT);
        let press = left && !*mouse_left_down;
        *mouse_left_down = left;
        if self.pending {
            self.status = Some("Waiting for theme store…".into());
            return BuilderEvent::None;
        }
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
        let layout = layout_for(inner);
        if layout.catalog.contains(mx, my) {
            let index = catalog_start(self.selected, layout.catalog.rows)
                .saturating_add(my.saturating_sub(inner.y));
            if index < self.catalog.len() {
                self.select(index);
            }
        } else if layout.editor.contains(mx, my) {
            let row = my.saturating_sub(inner.y);
            if row < TOKEN_ROWS {
                self.focus = row;
                self.editing = None;
            }
        } else if layout.apply.contains(mx, my) {
            return self.apply_event();
        }
        BuilderEvent::None
    }

    pub(crate) fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> BuilderEvent {
        if self.pending {
            self.status = Some("Waiting for theme store…".into());
            return BuilderEvent::None;
        }
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
                KeyCode::Char(c) if !c.is_control() => {
                    value.push(*c);
                }
                _ => {}
            }
            return BuilderEvent::None;
        }

        if let Some((role, original, cursor, was_edited)) = &mut self.editing {
            match key {
                KeyCode::Escape => {
                    set_role(&mut self.draft, *role, original.clone());
                    if !*was_edited {
                        self.edited.retain(|edited| edited != role);
                    }
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
                        remember_edit(&mut self.edited, *role);
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
                    remember_edit(&mut self.edited, *role);
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
                self.editing = Some((
                    role,
                    value.clone(),
                    value.len(),
                    self.edited.contains(&role),
                ));
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
        if self.requires_save {
            self.status = Some("Save the imported theme before applying it".into());
            return BuilderEvent::None;
        }
        if let Err(error) = self.draft.validate() {
            self.status = Some(error.to_string());
            return BuilderEvent::None;
        }
        self.pending = true;
        BuilderEvent::Apply {
            preset: self.active_name.clone(),
            theme: self.draft.clone(),
            overrides: self.edited_overrides().map(Box::new),
        }
    }

    fn select(&mut self, index: usize) {
        let Some(item) = self.catalog.get(index) else {
            return;
        };
        self.selected = index;
        self.active_name = item.name.clone();
        let palette = self
            .config
            .palette_with_user_themes(&self.active_name, &self.users);
        self.draft = UserTheme::from_palette(&self.active_name, &palette);
        self.requires_save = false;
        self.edited.clear();
        self.refresh_candidate();
        self.status = None;
    }

    fn refresh_candidate(&mut self) {
        let mut users = self.users.clone();
        if !users
            .iter()
            .any(|theme| theme.meta.name == self.active_name)
            && thegn_core::theme::preset(&self.active_name).is_none()
        {
            users.push(self.draft.clone());
        }
        let (colors, hues, accent, focus) = self.effective_overrides();
        self.candidate = thegn_core::theme_resolve::palette_with_catalog(
            &self.active_name,
            &users,
            &colors,
            &hues,
            &accent,
            &focus,
        );
        if self.draft.validate().is_err() {
            self.status = Some("Use #rgb or #rrggbb".into());
        }
    }

    fn effective_overrides(
        &self,
    ) -> (
        thegn_core::config::ThemeColors,
        thegn_core::config::ThemeHues,
        String,
        String,
    ) {
        let mut colors = self.config.theme.colors.clone();
        let mut hues = self.config.theme.hues.clone();
        let mut accent = self.config.theme.accent.clone();
        let mut focus = self.config.theme.focus_border.clone();
        for role in &self.edited {
            let value = role_value(&self.draft, *role);
            match role {
                ColorRole::Bg0 => colors.bg0 = Some(value),
                ColorRole::Bg1 => colors.bg1 = Some(value),
                ColorRole::Panel => colors.panel = Some(value),
                ColorRole::Panel2 => colors.panel2 = Some(value),
                ColorRole::Raise => colors.raise = Some(value),
                ColorRole::Border => colors.border = Some(value),
                ColorRole::Text => colors.text = Some(value),
                ColorRole::Dim => colors.dim = Some(value),
                ColorRole::Faint => colors.faint = Some(value),
                ColorRole::Ghost => colors.ghost = Some(value),
                ColorRole::Accent => accent = value,
                ColorRole::Focus => focus = value,
                ColorRole::Teal => hues.teal = Some(value),
                ColorRole::Magenta => hues.magenta = Some(value),
                ColorRole::Purple => hues.purple = Some(value),
                ColorRole::Green => hues.green = Some(value),
                ColorRole::Amber => hues.amber = Some(value),
                ColorRole::Red => hues.red = Some(value),
                ColorRole::Blue => hues.blue = Some(value),
                ColorRole::Orange => hues.orange = Some(value),
            }
        }
        (colors, hues, accent, focus)
    }

    fn edited_overrides(&self) -> Option<ThemeOverrides> {
        if self.edited.is_empty() {
            return None;
        }
        let mut overrides = ThemeOverrides::default();
        for role in &self.edited {
            let value = role_value(&self.draft, *role);
            match role {
                ColorRole::Bg0 => overrides.colors.bg0 = Some(value),
                ColorRole::Bg1 => overrides.colors.bg1 = Some(value),
                ColorRole::Panel => overrides.colors.panel = Some(value),
                ColorRole::Panel2 => overrides.colors.panel2 = Some(value),
                ColorRole::Raise => overrides.colors.raise = Some(value),
                ColorRole::Border => overrides.colors.border = Some(value),
                ColorRole::Text => overrides.colors.text = Some(value),
                ColorRole::Dim => overrides.colors.dim = Some(value),
                ColorRole::Faint => overrides.colors.faint = Some(value),
                ColorRole::Ghost => overrides.colors.ghost = Some(value),
                ColorRole::Accent => overrides.accent = Some(value),
                ColorRole::Focus => overrides.focus_border = Some(value),
                ColorRole::Teal => overrides.hues.teal = Some(value),
                ColorRole::Magenta => overrides.hues.magenta = Some(value),
                ColorRole::Purple => overrides.hues.purple = Some(value),
                ColorRole::Green => overrides.hues.green = Some(value),
                ColorRole::Amber => overrides.hues.amber = Some(value),
                ColorRole::Red => overrides.hues.red = Some(value),
                ColorRole::Blue => overrides.hues.blue = Some(value),
                ColorRole::Orange => overrides.hues.orange = Some(value),
            }
        }
        Some(overrides)
    }

    pub(crate) fn store_completed(&mut self, result: Result<UserTheme, String>) {
        self.pending = false;
        match result {
            Ok(theme) => {
                self.draft = theme;
                self.active_name = self.draft.meta.name.clone();
                self.requires_save = false;
                self.edited.clear();
                self.refresh_candidate();
                self.status = None;
            }
            Err(error) => self.status = Some(error),
        }
    }

    pub(crate) fn apply_completed(&mut self, result: Result<UserTheme, String>) {
        self.pending = false;
        match result {
            // Keep the palette that was actually confirmed live until the
            // config watcher reconciles the in-memory Config. Re-resolving
            // here would use the stale pre-write overrides and visibly undo a
            // successful token edit just as the popup closes.
            Ok(_) => self.status = None,
            Err(error) => self.status = Some(error),
        }
    }

    pub(crate) fn import_completed(&mut self, result: Result<UserTheme, String>) {
        self.pending = false;
        match result {
            Ok(theme) => {
                self.draft = theme;
                self.active_name = self.draft.meta.name.clone();
                self.requires_save = true;
                self.edited.clear();
                self.refresh_candidate();
                self.status = Some("Imported preview — Ctrl+S to save it".into());
            }
            Err(error) => self.status = Some(error),
        }
    }

    pub(crate) fn saved(&mut self, result: Result<UserTheme, String>) {
        match result {
            Ok(theme) => {
                self.store_completed(Ok(theme.clone()));
                self.active_name = theme.meta.name.clone();
                if let Some(index) = self
                    .catalog
                    .iter()
                    .position(|item| item.name == theme.meta.name)
                {
                    if !self.catalog[index].builtin {
                        self.catalog[index].theme = theme;
                        self.selected = index;
                    }
                } else {
                    self.catalog.push(CatalogItem {
                        name: theme.meta.name.clone(),
                        theme,
                        builtin: false,
                    });
                    self.selected = self.catalog.len() - 1;
                }
                self.status = Some("Theme saved".into());
            }
            Err(error) => self.store_completed(Err(error)),
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
}

fn catalog(cfg: &thegn_core::config::Config, users: &[UserTheme]) -> Vec<CatalogItem> {
    let mut out = thegn_core::theme::PRESETS
        .iter()
        .map(|name| CatalogItem {
            name: (*name).into(),
            theme: UserTheme::from_palette(*name, &cfg.palette_with_user_themes(name, users)),
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

pub(crate) fn cycle_catalog(
    cfg: &thegn_core::config::Config,
    users: &[UserTheme],
) -> Vec<(String, Palette)> {
    catalog(cfg, users)
        .into_iter()
        .map(|item| {
            let palette = cfg.palette_with_user_themes(&item.name, users);
            (item.name, palette)
        })
        .collect()
}

fn remember_edit(edited: &mut Vec<ColorRole>, role: ColorRole) {
    if !edited.contains(&role) {
        edited.push(role);
    }
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
        rows: ACTION_ROW + 1,
        anchor: Anchor::Center,
        bg: Tok::Slot(S::Panel),
        border: Tok::Slot(S::Focus),
        ..LayerSpec::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuilderLayout {
    catalog: Rect,
    editor: Rect,
    preview: Rect,
    apply: Rect,
}

fn layout_for(inner: Rect) -> BuilderLayout {
    let catalog_cols = CATALOG_COLS.min(inner.cols);
    let right_x = inner.x + catalog_cols.saturating_add(2).min(inner.cols);
    let right_cols = inner.cols.saturating_sub(right_x.saturating_sub(inner.x));
    let editor_cols = (right_cols / 2).max(1).min(right_cols);
    let preview_cols = right_cols.saturating_sub(editor_cols);
    let content_rows = ACTION_ROW.min(inner.rows.saturating_sub(1));
    BuilderLayout {
        catalog: Rect {
            x: inner.x,
            y: inner.y,
            cols: catalog_cols,
            rows: content_rows,
        },
        editor: Rect {
            x: right_x,
            y: inner.y,
            cols: editor_cols,
            rows: content_rows,
        },
        preview: Rect {
            x: right_x + editor_cols,
            y: inner.y,
            cols: preview_cols,
            rows: content_rows,
        },
        apply: Rect {
            x: right_x,
            y: inner.y + ACTION_ROW,
            cols: right_cols,
            rows: inner.rows.saturating_sub(ACTION_ROW).min(1),
        },
    }
}

fn catalog_start(selected: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 {
        0
    } else {
        selected.saturating_add(1).saturating_sub(visible_rows)
    }
}

impl ThemeBuilder {
    pub(crate) fn render(&self, surface: &mut Surface, screen: Rect) {
        let Some(inner) = open_layer(surface, screen, &layer_spec()) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        let bar = if self
            .catalog
            .get(self.selected)
            .is_some_and(|item| item.name == thegn_core::theme::PRESETS[0])
        {
            Bar::Default
        } else {
            Bar::Preset
        };
        let findings = theme_contrast::audit(&self.candidate, bar);
        let contrast = if findings.is_empty() {
            "contrast ok".into()
        } else {
            format!("contrast warning ({})", findings.len())
        };
        let layout = layout_for(inner);
        let catalog_start = catalog_start(self.selected, layout.catalog.rows);
        for (row, item) in self
            .catalog
            .iter()
            .skip(catalog_start)
            .take(layout.catalog.rows)
            .enumerate()
        {
            let y = layout.catalog.y + row;
            let selected = catalog_start + row == self.selected;
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
                layout.catalog.cols,
                &Line::segs(vec![
                    seg(fg, format!("{marker} {}", item.name)),
                    sp(1),
                    seg(Tok::Slot(S::Ghost), kind),
                ]),
                bg,
            );
        }
        for (row, (role, name)) in ROLES.iter().enumerate() {
            let y = layout.editor.y + row;
            if y >= layout.editor.y + layout.editor.rows {
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
            let prefix = if focused {
                format!("{} ", crate::caps::glyph(crate::caps::Glyph::ArrowRight))
            } else {
                "  ".to_string()
            };
            seg::draw_line(
                surface,
                layout.editor.x,
                y,
                layout.editor.cols,
                &Line::segs(vec![
                    seg(fg, format!("{prefix}{name:<8}")),
                    seg(role_token(*role), value),
                ]),
                bg,
            );
        }
        if layout.apply.rows > 0 {
            let bg = if self.focus == ACTION_ROW {
                Tok::SelAccent
            } else {
                panel
            };
            seg::draw_line(
                surface,
                layout.apply.x,
                layout.apply.y,
                layout.apply.cols,
                &Line::segs(vec![
                    Seg::key(" Apply "),
                    sp(1),
                    seg(Tok::Slot(S::Text), "Enter"),
                    sp(2),
                    seg(
                        if findings.is_empty() {
                            Tok::Slot(S::ActivityDone)
                        } else {
                            Tok::Hue(Hue::Amber)
                        },
                        contrast,
                    ),
                    sp(2),
                    seg(Tok::Slot(S::Dim), "Ctrl+S save as · i import"),
                ]),
                bg,
            );
        }
        render_preview(surface, layout.preview, panel);
        if let Some((mode, value)) = &self.input {
            let label = match mode {
                InputMode::SaveName => "save as name",
                InputMode::ImportPath => "import local path",
            };
            let y = layout.preview.y + layout.preview.rows.saturating_sub(1);
            if layout.preview.rows > 0 {
                seg::draw_line(
                    surface,
                    layout.preview.x,
                    y,
                    layout.preview.cols,
                    &Line::segs(vec![
                        seg(Tok::Slot(S::Accent), format!("{label}: ")).bold(),
                        seg(Tok::Slot(S::Text), value.to_string()).into_caret(),
                    ]),
                    Tok::Slot(S::Raise),
                );
            }
        } else if let Some(status) = &self.status {
            let y = layout.preview.y + layout.preview.rows.saturating_sub(1);
            if layout.preview.rows > 0 {
                seg::draw_line(
                    surface,
                    layout.preview.x,
                    y,
                    layout.preview.cols,
                    &Line::segs(vec![seg(Tok::Hue(Hue::Red), status.clone())]),
                    panel,
                );
            }
        }
    }
}

fn render_preview(surface: &mut Surface, preview: Rect, panel: Tok) {
    if preview.rows == 0 || preview.cols == 0 {
        return;
    }
    let x = preview.x;
    let y = preview.y;
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
            seg(Tok::Hue(Hue::Purple), dot),
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
            seg(Tok::Slot(S::Ghost3), format!("{dot}{dot}{dot}")),
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
        if y + row >= preview.y + preview.rows {
            break;
        }
        seg::draw_line(surface, x, y + row, preview.cols, line, panel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> ThemeBuilder {
        ThemeBuilder::open(&thegn_core::config::Config::default(), &[])
    }

    #[test]
    fn catalog_keeps_builtins_ahead_of_users_and_builtin_names_win() {
        let cfg = thegn_core::config::Config::default();
        let user = UserTheme::from_palette(
            thegn_core::theme::PRESETS[0],
            &cfg.palette_with_preset(thegn_core::theme::PRESETS[0]),
        );
        let items = catalog(&cfg, &[user]);
        assert_eq!(items.len(), thegn_core::theme::PRESETS.len());
        assert!(items[0].builtin);
    }

    #[test]
    fn color_edit_cancel_restores_the_snapshot() {
        let mut b = builder();
        let original = role_value(&b.draft, ColorRole::Bg0);
        b.editing = Some((ColorRole::Bg0, original.clone(), original.len(), false));
        set_role(&mut b.draft, ColorRole::Bg0, "#ffffff".into());
        remember_edit(&mut b.edited, ColorRole::Bg0);
        b.refresh_candidate();
        assert!(b.is_dirty());
        assert_eq!(
            b.handle_key(&KeyCode::Escape, Modifiers::NONE),
            BuilderEvent::None
        );
        assert_eq!(b.cancel_palette(), b.snapshot);
        assert_eq!(role_value(&b.draft, ColorRole::Bg0), original);
        assert!(b.edited.is_empty());
    }

    #[test]
    fn pasted_path_is_data_and_control_characters_are_removed() {
        let mut b = builder();
        b.input = Some((InputMode::ImportPath, String::new()));
        assert!(b.handle_paste(" /tmp/theme.yml\n"));
        assert_eq!(b.input_value_for_test(), Some("/tmp/theme.yml"));
    }

    #[test]
    fn small_render_is_safe_and_apply_is_an_explicit_action() {
        let b = builder();
        let mut surface = Surface::new(80, 24);
        b.render(&mut surface, Rect::full(80, 24));
        let mut b = builder();
        b.focus = ACTION_ROW;
        assert!(matches!(
            b.handle_key(&KeyCode::Enter, Modifiers::NONE),
            BuilderEvent::Apply { .. }
        ));
    }

    #[test]
    fn layout_keeps_apply_and_preview_inside_the_popup_without_overlap() {
        for screen in [Rect::full(80, 24), Rect::full(140, 40)] {
            let outer = box_rect(&layer_spec(), screen).expect("popup fits");
            let inner = Rect {
                x: outer.x + 2,
                y: outer.y + 1,
                cols: outer.cols - 4,
                rows: outer.rows - 2,
            };
            let layout = layout_for(inner);
            assert!(layout.apply.rows > 0);
            assert!(outer.contains(layout.apply.x, layout.apply.y));
            assert!(outer.contains(
                layout.preview.x + layout.preview.cols.saturating_sub(1),
                layout.preview.y + 5,
            ));
            assert!(layout.preview.y + layout.preview.rows <= layout.apply.y);

            let mut b = builder();
            let event = MouseEvent {
                x: layout.apply.x as u16,
                y: layout.apply.y as u16,
                mouse_buttons: MouseButtons::LEFT,
                modifiers: Modifiers::NONE,
            };
            let mut left_down = false;
            assert!(matches!(
                b.handle_mouse(
                    &event,
                    layout.apply.x,
                    layout.apply.y,
                    screen,
                    false,
                    &mut left_down
                ),
                BuilderEvent::Apply { .. }
            ));
        }
    }

    #[test]
    fn render_keeps_contrast_feedback_visible_at_eighty_columns() {
        let b = builder();
        let mut surface = Surface::new(80, 24);
        b.render(&mut surface, Rect::full(80, 24));
        assert!(surface.screen_chars_to_string().contains("contrast"));
    }

    #[test]
    fn catalog_scrolls_to_keep_the_selected_user_theme_visible() {
        let mut cfg = thegn_core::config::Config::default();
        let users = (0..30)
            .map(|index| UserTheme::from_palette(format!("user-{index:02}"), &cfg.palette()))
            .collect::<Vec<_>>();
        cfg.theme.preset = "user-29".into();
        let b = ThemeBuilder::open(&cfg, &users);
        let mut surface = Surface::new(80, 24);
        b.render(&mut surface, Rect::full(80, 24));
        assert!(surface.screen_chars_to_string().contains("user-29"));
    }

    #[test]
    fn pending_apply_cannot_be_dismissed_and_keeps_confirmed_palette() {
        let mut b = builder();
        b.set_role_for_test(ColorRole::Bg0, "#abcdef");
        remember_edit(&mut b.edited, ColorRole::Bg0);
        b.refresh_candidate();
        let confirmed = b.candidate.clone();
        let applied_theme = b.draft.clone();
        assert!(matches!(b.apply_event(), BuilderEvent::Apply { .. }));
        assert_eq!(
            b.handle_key(&KeyCode::Escape, Modifiers::NONE),
            BuilderEvent::None
        );
        b.apply_completed(Ok(applied_theme));
        assert_eq!(b.candidate, confirmed);
        assert!(b.status().is_none());
    }

    #[test]
    fn imported_preview_must_be_saved_before_it_can_be_applied() {
        let mut b = builder();
        let imported = UserTheme::from_palette("imported", b.candidate());
        b.import_completed(Ok(imported.clone()));
        assert_eq!(b.apply_event(), BuilderEvent::None);
        assert_eq!(
            b.status(),
            Some("Save the imported theme before applying it")
        );
        b.saved(Ok(imported));
        assert!(matches!(b.apply_event(), BuilderEvent::Apply { .. }));
    }

    #[test]
    fn token_edit_persists_only_the_intentionally_edited_override() {
        let mut b = builder();
        b.set_role_for_test(ColorRole::Bg0, "#abcdef");
        remember_edit(&mut b.edited, ColorRole::Bg0);
        let overrides = b.edited_overrides().expect("edited token");
        assert_eq!(overrides.colors.bg0.as_deref(), Some("#abcdef"));
        assert!(overrides.colors.text.is_none());
        assert!(overrides.hues.teal.is_none());
        assert!(overrides.accent.is_none());
    }

    impl ThemeBuilder {
        fn input_value_for_test(&self) -> Option<&str> {
            self.input.as_ref().map(|(_, value)| value.as_str())
        }

        fn set_role_for_test(&mut self, role: ColorRole, value: &str) {
            set_role(&mut self.draft, role, value.into());
        }
    }
}
