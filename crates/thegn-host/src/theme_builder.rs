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

mod elements;
mod picker;

use elements::{ColorRole, ELEMENT_GROUPS, ElementRow, ROLES, element_rows, role_name, row_role};
use picker::{GridLine, Move, PickerSection, PickerState};

const CATALOG_COLS: usize = 27;
const TOKEN_ROWS: usize = ROLES.len();
const ACTION_ROW: usize = TOKEN_ROWS;
/// Preview-column row of the selected role's contrast line (under the six
/// sample rows).
const CONTRAST_ROW: usize = 7;

/// What the editor column lists: the raw palette tokens, or the UI elements
/// each token paints. `v` toggles it when no field is being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorView {
    Tokens,
    Elements,
}

#[derive(Debug, Clone)]
pub(crate) struct CatalogItem {
    pub name: String,
    pub theme: UserTheme,
    pub builtin: bool,
    /// The theme's resolved palette, computed once when the catalog is built
    /// so the swatch strip never re-derives it per frame.
    pub palette: Palette,
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
    view: EditorView,
    /// Per-group collapsed flags of the element view (missing = expanded).
    collapsed: Vec<bool>,
    /// Element-view cursor: an index into `element_rows(&collapsed)`, or one
    /// past the end for the Apply row.
    elem_cursor: usize,
    /// Wheel scroll offsets; `None` follows the list's cursor.
    elem_scroll: Option<usize>,
    catalog_scroll: Option<usize>,
    picker: Option<PickerState>,
    /// The picker's cells, captured when it opened so the "theme" section
    /// does not reshuffle while its own moves preview new values.
    picker_cells: Vec<PickerSection>,
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
            view: EditorView::Tokens,
            collapsed: vec![false; ELEMENT_GROUPS.len()],
            elem_cursor: 0,
            elem_scroll: None,
            catalog_scroll: None,
            picker: None,
            picker_cells: Vec::new(),
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
        let inner = Rect {
            x: outer.x + 2,
            y: outer.y + 1,
            cols: outer.cols.saturating_sub(4),
            rows: outer.rows.saturating_sub(2),
        };
        let layout = layout_for(inner);
        // The loop forwards every mouse event; the reducer decides what a
        // wheel tick means: it scrolls whichever list sits under the pointer.
        if event.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
            let down = !event.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE);
            self.scroll_at(layout, mx, my, down);
            return BuilderEvent::None;
        }
        if !press {
            return BuilderEvent::None;
        }
        if self.picker.is_some() {
            // The picker is a sub-modal: only its own cells take clicks, so a
            // stray click cannot swap the theme under an open preview.
            if let Some(cursor) = self.picker_hit(layout.editor, mx, my) {
                self.click_picker_cell(cursor);
            }
            return BuilderEvent::None;
        }
        if layout.catalog.contains(mx, my) {
            let start = list_start(
                self.catalog_scroll,
                self.selected,
                layout.catalog.rows,
                self.catalog.len(),
            );
            let index = start.saturating_add(my.saturating_sub(layout.catalog.y));
            if index < self.catalog.len() {
                self.select(index);
            }
        } else if layout.editor.contains(mx, my) {
            let row = my.saturating_sub(layout.editor.y);
            match self.view {
                EditorView::Tokens => {
                    if row < TOKEN_ROWS {
                        self.focus = row;
                        self.editing = None;
                    }
                }
                EditorView::Elements => self.click_element(layout.editor, row, mx),
            }
        } else if layout.apply.contains(mx, my) {
            return self.apply_event();
        }
        BuilderEvent::None
    }

    /// One wheel tick over the catalog, the element list, or the open picker.
    fn scroll_at(&mut self, layout: BuilderLayout, mx: usize, my: usize, down: bool) {
        if layout.catalog.contains(mx, my) {
            let (rows, len) = (layout.catalog.rows, self.catalog.len());
            let start = list_start(self.catalog_scroll, self.selected, rows, len);
            self.catalog_scroll = Some(wheel_step(start, down, len, rows));
        } else if layout.editor.contains(mx, my)
            && let Some(state) = &self.picker
        {
            let lines = picker::grid_lines(&self.picker_cells);
            let visible = picker_rows(layout.editor);
            let cursor = picker::cursor_line(&lines, &self.picker_cells, state.cursor);
            let start = list_start(state.scroll, cursor, visible, lines.len());
            let next = wheel_step(start, down, lines.len(), visible);
            if let Some(state) = self.picker.as_mut() {
                state.scroll = Some(next);
            }
        } else if layout.editor.contains(mx, my) && self.view == EditorView::Elements {
            let rows = element_rows(&self.collapsed);
            let start = self.elements_start(rows.len(), layout.editor.rows);
            self.elem_scroll = Some(wheel_step(start, down, rows.len(), layout.editor.rows));
        }
    }

    fn elements_start(&self, len: usize, visible: usize) -> usize {
        list_start(
            self.elem_scroll,
            self.elem_cursor.min(len.saturating_sub(1)),
            visible,
            len,
        )
    }

    fn click_element(&mut self, editor: Rect, row: usize, mx: usize) {
        let rows = element_rows(&self.collapsed);
        let index = self.elements_start(rows.len(), editor.rows) + row;
        let Some(hit) = rows.get(index).copied() else {
            return;
        };
        self.editing = None;
        match hit {
            ElementRow::Group(group) => self.toggle_group(group, None),
            ElementRow::Element(..) => {
                self.elem_cursor = index;
                // Columns 2..4 hold the two-cell swatch (after the marker).
                let on_swatch = mx >= editor.x + 2 && mx < editor.x + 4;
                if on_swatch && let Some(role) = row_role(hit) {
                    self.open_picker(role);
                }
            }
        }
    }

    /// Collapse (`Some(false)`), expand (`Some(true)`) or toggle (`None`) a
    /// group; the cursor lands on its header.
    fn toggle_group(&mut self, group: usize, expand: Option<bool>) {
        if self.collapsed.len() < ELEMENT_GROUPS.len() {
            self.collapsed.resize(ELEMENT_GROUPS.len(), false);
        }
        let Some(flag) = self.collapsed.get_mut(group) else {
            return;
        };
        *flag = expand.map_or(!*flag, |expand| !expand);
        self.elem_cursor = element_rows(&self.collapsed)
            .iter()
            .position(|row| *row == ElementRow::Group(group))
            .unwrap_or(0);
    }

    /// The picker cell under the pointer, if any.
    fn picker_hit(&self, editor: Rect, mx: usize, my: usize) -> Option<(usize, usize)> {
        let state = self.picker.as_ref()?;
        if !editor.contains(mx, my) || my == editor.y {
            return None;
        }
        let lines = picker::grid_lines(&self.picker_cells);
        let visible = picker_rows(editor);
        let cursor = picker::cursor_line(&lines, &self.picker_cells, state.cursor);
        let start = list_start(state.scroll, cursor, visible, lines.len());
        let GridLine::Cells { section, row } = *lines.get(start + (my - editor.y - 1))? else {
            return None;
        };
        let block = self.picker_cells.get(section)?;
        let column = (mx - editor.x) / picker_cell_width(editor.cols);
        if column >= block.cols {
            return None;
        }
        let index = row * block.cols + column;
        (index < block.cells.len()).then_some((section, index))
    }

    /// A first click previews a cell; a second click on the previewed cell
    /// keeps it (the mouse twin of Enter).
    fn click_picker_cell(&mut self, cursor: (usize, usize)) {
        let Some(state) = self.picker.as_ref() else {
            return;
        };
        let previewed = state.cursor == cursor
            && picker::cell(&self.picker_cells, cursor).is_some_and(|cell| {
                cell.eq_ignore_ascii_case(&role_value(&self.draft, state.role))
            });
        if previewed {
            self.picker = None;
        } else {
            self.move_picker_to(cursor, false);
        }
    }

    /// Move the picker cursor and preview its value live: the role is written
    /// into the draft and recorded as an intentional edit, exactly as typing a
    /// hex value does. `follow` re-pins the scroll to the cursor (keys do;
    /// clicks keep the wheel position).
    fn move_picker_to(&mut self, cursor: (usize, usize), follow: bool) {
        let Some(value) = picker::cell(&self.picker_cells, cursor).map(str::to_owned) else {
            return;
        };
        let Some(state) = self.picker.as_mut() else {
            return;
        };
        state.cursor = cursor;
        if follow {
            state.scroll = None;
        }
        let role = state.role;
        set_role(&mut self.draft, role, value);
        remember_edit(&mut self.edited, role);
        self.refresh_candidate();
    }

    fn open_picker(&mut self, role: ColorRole) {
        let value = role_value(&self.draft, role);
        self.picker_cells = picker::palette_cells(&self.candidate);
        let cursor = picker::locate(&self.picker_cells, &value).unwrap_or((0, 0));
        self.picker = Some(PickerState {
            role,
            was_edited: self.edited.contains(&role),
            original: value,
            cursor,
            scroll: None,
        });
        self.editing = None;
        self.status = None;
    }

    /// Undo an in-progress edit of `role`: the hex editor's and the picker's
    /// shared Esc path.
    fn restore_role(&mut self, role: ColorRole, original: String, was_edited: bool) {
        set_role(&mut self.draft, role, original);
        if !was_edited {
            self.edited.retain(|edited| *edited != role);
        }
        self.refresh_candidate();
    }

    fn picker_key(&mut self, key: &KeyCode) {
        let Some(state) = self.picker.as_ref() else {
            return;
        };
        let (role, cursor) = (state.role, state.cursor);
        let dir = match key {
            KeyCode::Escape => {
                if let Some(state) = self.picker.take() {
                    self.restore_role(role, state.original, state.was_edited);
                }
                return;
            }
            KeyCode::Enter => {
                self.picker = None;
                return;
            }
            KeyCode::Char('#') => {
                if let Some(state) = self.picker.take() {
                    let len = role_value(&self.draft, role).len();
                    self.editing = Some((role, state.original, len, state.was_edited));
                }
                return;
            }
            KeyCode::LeftArrow | KeyCode::Char('h') => Move::Left,
            KeyCode::RightArrow | KeyCode::Char('l') => Move::Right,
            KeyCode::UpArrow | KeyCode::Char('k') => Move::Up,
            KeyCode::DownArrow | KeyCode::Char('j') => Move::Down,
            _ => return,
        };
        let next = picker::step(&self.picker_cells, cursor, dir);
        if next != cursor {
            self.move_picker_to(next, true);
        }
    }

    /// Element-view keys; `None` falls through to the shared top-level keys
    /// (catalog arrows, Esc, `i`).
    fn element_key(&mut self, key: &KeyCode, mods: Modifiers) -> Option<BuilderEvent> {
        let rows = element_rows(&self.collapsed);
        let apply = rows.len();
        self.elem_cursor = self.elem_cursor.min(apply);
        let current = rows.get(self.elem_cursor).copied();
        match key {
            KeyCode::Tab => {
                self.elem_cursor = if !mods.contains(Modifiers::SHIFT) {
                    (self.elem_cursor + 1).min(apply)
                } else if self.elem_cursor == 0 {
                    apply
                } else {
                    self.elem_cursor - 1
                };
                self.elem_scroll = None;
            }
            KeyCode::Enter if current.is_none() => return Some(self.apply_event()),
            KeyCode::Enter | KeyCode::Char(' ') => match current {
                Some(ElementRow::Group(group)) => self.toggle_group(group, None),
                Some(row) => {
                    if let Some(role) = row_role(row) {
                        self.open_picker(role);
                    }
                }
                None => {}
            },
            KeyCode::LeftArrow => {
                if let Some(ElementRow::Group(group) | ElementRow::Element(group, _)) = current {
                    self.toggle_group(group, Some(false));
                }
            }
            KeyCode::RightArrow => {
                if let Some(ElementRow::Group(group)) = current {
                    self.toggle_group(group, Some(true));
                }
            }
            _ => return None,
        }
        Some(BuilderEvent::None)
    }

    fn apply_focused(&self) -> bool {
        match self.view {
            EditorView::Tokens => self.focus == ACTION_ROW,
            EditorView::Elements => self.elem_cursor >= element_rows(&self.collapsed).len(),
        }
    }

    /// The role the contrast line describes: the one being picked or typed,
    /// else the focused token/element.
    fn selected_role(&self) -> Option<ColorRole> {
        if let Some(state) = &self.picker {
            return Some(state.role);
        }
        if let Some((role, ..)) = &self.editing {
            return Some(*role);
        }
        match self.view {
            EditorView::Tokens => ROLES.get(self.focus).map(|(role, _)| *role),
            EditorView::Elements => element_rows(&self.collapsed)
                .get(self.elem_cursor)
                .copied()
                .and_then(row_role),
        }
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
                    let (role, original, was_edited) = (*role, original.clone(), *was_edited);
                    self.editing = None;
                    self.restore_role(role, original, was_edited);
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
                KeyCode::Char(c) if is_hex_input(*c) => {
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

        if self.picker.is_some() {
            self.picker_key(key);
            return BuilderEvent::None;
        }

        if mods.contains(Modifiers::CTRL) && matches!(key, KeyCode::Char('s' | 'S')) {
            self.input = Some((InputMode::SaveName, self.draft.meta.name.clone()));
            self.status = None;
            return BuilderEvent::None;
        }
        if mods.is_empty() && matches!(key, KeyCode::Char('v')) {
            self.view = match self.view {
                EditorView::Tokens => EditorView::Elements,
                EditorView::Elements => EditorView::Tokens,
            };
            self.elem_scroll = None;
            return BuilderEvent::None;
        }
        if self.view == EditorView::Elements
            && let Some(event) = self.element_key(key, mods)
        {
            return event;
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
                self.open_picker(role);
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
        self.catalog_scroll = None;
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
                        self.catalog[index].palette = thegn_core::theme_user::to_palette(&theme);
                        self.catalog[index].theme = theme;
                        self.selected = index;
                    }
                } else {
                    self.catalog.push(CatalogItem {
                        name: theme.meta.name.clone(),
                        palette: thegn_core::theme_user::to_palette(&theme),
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
        .map(|name| {
            let palette = cfg.palette_with_user_themes(name, users);
            CatalogItem {
                name: (*name).into(),
                theme: UserTheme::from_palette(*name, &palette),
                builtin: true,
                palette,
            }
        })
        .collect::<Vec<_>>();
    for user in users {
        if !out.iter().any(|item| item.name == user.meta.name) {
            out.push(CatalogItem {
                name: user.meta.name.clone(),
                theme: user.clone(),
                builtin: false,
                palette: thegn_core::theme_user::to_palette(user),
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

fn is_hex_input(character: char) -> bool {
    character == '#' || character.is_ascii_hexdigit()
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
        // The Apply row sits directly under the content, so a short screen
        // that clips the token list still keeps the explicit Apply action.
        apply: Rect {
            x: right_x,
            y: inner.y + content_rows,
            cols: right_cols,
            rows: inner.rows.saturating_sub(content_rows).min(1),
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

/// The first visible index of a `len`-row list showing `visible` rows: the
/// wheel offset when one is set, else the `catalog_start` follow-the-cursor
/// position, clamped so the list never scrolls past its end.
fn list_start(scroll: Option<usize>, cursor: usize, visible: usize, len: usize) -> usize {
    scroll
        .unwrap_or_else(|| catalog_start(cursor, visible))
        .min(len.saturating_sub(visible))
}

fn wheel_step(start: usize, down: bool, len: usize, visible: usize) -> usize {
    if down {
        start.saturating_add(1).min(len.saturating_sub(visible))
    } else {
        start.saturating_sub(1)
    }
}

/// Grid lines the picker shows: the editor column minus its title row.
fn picker_rows(editor: Rect) -> usize {
    editor.rows.saturating_sub(1)
}

/// Columns per picker cell: a two-cell swatch plus a gap when the column is
/// wide enough for twelve of them, a single cell on a narrow popup.
fn picker_cell_width(cols: usize) -> usize {
    (cols / picker::MAX_COLS).clamp(1, 3)
}

fn palette_value(p: &Palette, role: ColorRole) -> &str {
    match role {
        ColorRole::Bg0 => &p.bg0,
        ColorRole::Bg1 => &p.bg1,
        ColorRole::Panel => &p.panel,
        ColorRole::Panel2 => &p.panel2,
        ColorRole::Raise => &p.raise,
        ColorRole::Border => &p.border,
        ColorRole::Text => &p.text,
        ColorRole::Dim => &p.dim,
        ColorRole::Faint => &p.faint,
        ColorRole::Ghost => &p.ghost,
        ColorRole::Accent => &p.accent,
        ColorRole::Focus => &p.focus,
        ColorRole::Teal => &p.hues.teal,
        ColorRole::Magenta => &p.hues.magenta,
        ColorRole::Purple => &p.hues.purple,
        ColorRole::Green => &p.hues.green,
        ColorRole::Amber => &p.hues.amber,
        ColorRole::Red => &p.hues.red,
        ColorRole::Blue => &p.hues.blue,
        ColorRole::Orange => &p.hues.orange,
    }
}

/// The selected role's contrast as two short lines plus their token: the
/// worst failing contract pair that involves the role (amber, red below 3:1),
/// else its plain ratio against the surface it is read on (text for a
/// surface role, bg0 for everything else).
fn role_contrast(
    p: &Palette,
    role: ColorRole,
    findings: &[theme_contrast::ContrastFinding],
) -> (String, String, Tok) {
    let name = role_name(role);
    let worst = findings
        .iter()
        .filter(|finding| finding.fg == name || finding.bg == name)
        .min_by(|a, b| a.ratio.total_cmp(&b.ratio));
    if let Some(finding) = worst {
        let other = if finding.fg == name {
            finding.bg
        } else {
            finding.fg
        };
        let tok = if finding.ratio < 3.0 {
            Tok::Hue(Hue::Red)
        } else {
            Tok::Hue(Hue::Amber)
        };
        return (
            format!("{name} contrast {:.1}:1", finding.ratio),
            format!("against {other}, below {:.1}:1", finding.min),
            tok,
        );
    }
    let value = palette_value(p, role);
    let surface = matches!(
        role,
        ColorRole::Bg0 | ColorRole::Bg1 | ColorRole::Panel | ColorRole::Panel2 | ColorRole::Raise
    );
    let (other, ratio) = if surface {
        ("text", thegn_core::theme::contrast_ratio(&p.text, value))
    } else {
        ("bg0", thegn_core::theme::contrast_ratio(value, &p.bg0))
    };
    (
        format!("{name} contrast {ratio:.1}:1"),
        format!("against {other}, ok"),
        Tok::Slot(S::Dim),
    )
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
        let catalog_start = list_start(
            self.catalog_scroll,
            self.selected,
            layout.catalog.rows,
            self.catalog.len(),
        );
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
            let strip = strip_width(layout.catalog.cols);
            let name_cols = layout
                .catalog
                .cols
                .saturating_sub(strip + usize::from(strip > 0));
            // Marker + space take 2 cols and one col separates the kind, so the
            // kind stays visible however long the name is.
            let name = seg::clip_end(
                &item.name,
                name_cols.saturating_sub(UnicodeWidthStr::width(kind) + 3),
            );
            seg::draw_line(
                surface,
                inner.x,
                y,
                name_cols,
                &Line::Split {
                    l: vec![seg(fg, format!("{marker} {name}"))],
                    r: vec![seg(Tok::Slot(S::Ghost), kind)],
                },
                bg,
            );
            if strip > 0 {
                let cells = swatch_strip(&item.palette)
                    .into_iter()
                    .map(|fill| seg(Tok::Slot(S::Text), " ").bg(fill))
                    .collect::<Vec<_>>();
                seg::draw_line(
                    surface,
                    inner.x + layout.catalog.cols - strip,
                    y,
                    strip,
                    &Line::segs(cells),
                    bg,
                );
            }
        }
        if self.picker.is_some() {
            self.render_picker(surface, layout.editor, panel);
        } else if self.view == EditorView::Elements {
            self.render_elements(surface, layout.editor, panel);
        } else {
            self.render_tokens(surface, layout.editor, panel);
        }
        if layout.apply.rows > 0 {
            let bg = if self.apply_focused() {
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
                    seg(Tok::Slot(S::Dim), "Ctrl+S save as · i import · v view"),
                ]),
                bg,
            );
        }
        let highlight = self.selected_role();
        render_preview(surface, layout.preview, panel, highlight);
        if let Some(role) = highlight
            && layout.preview.rows > CONTRAST_ROW + 2
        {
            seg::draw_line(
                surface,
                layout.preview.x,
                layout.preview.y + CONTRAST_ROW - 1,
                layout.preview.cols,
                &Line::segs(vec![seg(Tok::Slot(S::Dim), preview_legend(role))]),
                panel,
            );
        }
        // The selected role's own contrast, under the preview sample; the
        // status/input line keeps the column's last row.
        if let Some(role) = self.selected_role()
            && layout.preview.rows > CONTRAST_ROW + 2
        {
            let (headline, detail, tok) = role_contrast(&self.candidate, role, &findings);
            for (offset, text) in [headline, detail].into_iter().enumerate() {
                seg::draw_line(
                    surface,
                    layout.preview.x,
                    layout.preview.y + CONTRAST_ROW + offset,
                    layout.preview.cols,
                    &Line::segs(vec![seg(tok, text)]),
                    panel,
                );
            }
        }
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

    fn render_tokens(&self, surface: &mut Surface, editor: Rect, panel: Tok) {
        for (row, (role, name)) in ROLES.iter().enumerate() {
            let y = editor.y + row;
            if y >= editor.y + editor.rows {
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
                editor.x,
                y,
                editor.cols,
                &Line::segs(vec![
                    seg(fg, format!("{prefix}{name:<8}")),
                    seg(role_token(*role), value),
                ]),
                bg,
            );
        }
    }

    fn render_elements(&self, surface: &mut Surface, editor: Rect, panel: Tok) {
        let rows = element_rows(&self.collapsed);
        let start = self.elements_start(rows.len(), editor.rows);
        for (index, row) in rows.iter().enumerate().skip(start).take(editor.rows) {
            let y = editor.y + index - start;
            let focused = index == self.elem_cursor;
            let bg = if focused {
                Tok::Sel(Hue::Blue, 28)
            } else {
                panel
            };
            let marker = if focused {
                format!("{} ", crate::caps::glyph(crate::caps::Glyph::ArrowRight))
            } else {
                "  ".to_string()
            };
            let line = match *row {
                ElementRow::Group(group) => {
                    let caret = if self.collapsed.get(group).copied().unwrap_or(false) {
                        crate::caps::Glyph::CaretClosed
                    } else {
                        crate::caps::Glyph::CaretOpen
                    };
                    Line::segs(vec![
                        seg(Tok::Slot(S::Text), marker),
                        seg(
                            Tok::Slot(S::Accent),
                            format!("{} {}", crate::caps::glyph(caret), ELEMENT_GROUPS[group].0),
                        )
                        .bold(),
                    ])
                }
                ElementRow::Element(group, element) => {
                    let (label, role) = ELEMENT_GROUPS[group].1[element];
                    let swatch = role_token(role);
                    let fg = if focused {
                        Tok::Slot(S::Text)
                    } else {
                        Tok::Slot(S::Dim)
                    };
                    Line::segs(vec![
                        seg(Tok::Slot(S::Text), marker),
                        seg(swatch, "  ").bg(swatch),
                        sp(1),
                        seg(fg, format!("{label:<14}")),
                        seg(swatch, role_name(role)),
                    ])
                }
            };
            seg::draw_line(surface, editor.x, y, editor.cols, &line, bg);
        }
    }

    fn render_picker(&self, surface: &mut Surface, editor: Rect, panel: Tok) {
        let Some(state) = &self.picker else {
            return;
        };
        if editor.rows == 0 {
            return;
        }
        let swatch = role_token(state.role);
        seg::draw_line(
            surface,
            editor.x,
            editor.y,
            editor.cols,
            &Line::segs(vec![
                seg(Tok::Slot(S::Accent), format!("{} ", role_name(state.role))).bold(),
                seg(swatch, "  ").bg(swatch),
                sp(1),
                seg(Tok::Slot(S::Text), role_value(&self.draft, state.role)),
                sp(2),
                seg(Tok::Slot(S::Dim), "Enter keep · Esc revert · # hex"),
            ]),
            panel,
        );
        let lines = picker::grid_lines(&self.picker_cells);
        let visible = picker_rows(editor);
        let cursor_line = picker::cursor_line(&lines, &self.picker_cells, state.cursor);
        let start = list_start(state.scroll, cursor_line, visible, lines.len());
        let width = picker_cell_width(editor.cols);
        let swatch_width = if width >= 3 { 2 } else { width };
        let mark = crate::caps::glyph(crate::caps::Glyph::DotFilled);
        for (offset, line) in lines.iter().skip(start).take(visible).enumerate() {
            let y = editor.y + 1 + offset;
            let rendered = match *line {
                GridLine::Header(section) => Line::segs(vec![seg(
                    Tok::Slot(S::Faint),
                    self.picker_cells[section].name,
                )]),
                GridLine::Cells { section, row } => {
                    let block = &self.picker_cells[section];
                    let mut segs = Vec::with_capacity(block.cols * 2);
                    for (index, value) in block
                        .cells
                        .iter()
                        .enumerate()
                        .skip(row * block.cols)
                        .take(block.cols)
                    {
                        let Some((r, g, b)) = picker::parse_hex(value) else {
                            continue;
                        };
                        let fill = Tok::Rgb(r, g, b);
                        let text = if (section, index) == state.cursor {
                            format!("{mark:<swatch_width$}")
                        } else {
                            " ".repeat(swatch_width)
                        };
                        // The cursor mark takes whichever theme token reads
                        // better on this cell.
                        let frag = format!("{r};{g};{b}");
                        let ink = if thegn_core::theme::contrast_ratio(&frag, &self.candidate.text)
                            >= thegn_core::theme::contrast_ratio(&frag, &self.candidate.bg0)
                        {
                            Tok::Slot(S::Text)
                        } else {
                            Tok::Slot(S::Bg0)
                        };
                        segs.push(seg(ink, text).bg(fill).bold());
                        if width > swatch_width {
                            segs.push(sp(width - swatch_width));
                        }
                    }
                    Line::segs(segs)
                }
            };
            seg::draw_line(surface, editor.x, y, editor.cols, &rendered, panel);
        }
    }
}

/// Catalog swatch strip: bg0, panel, accent, focus, green, red.
const STRIP_CELLS: usize = 6;
/// Narrowest catalog column that still has room for a name beside the strip.
const STRIP_MIN_COLS: usize = 24;

/// Cells the swatch strip takes in a catalog column of `cols` (0 = dropped).
fn strip_width(cols: usize) -> usize {
    if cols >= STRIP_MIN_COLS {
        STRIP_CELLS
    } else {
        0
    }
}

/// A palette token string (`r;g;b`) as a fill; unparsable values fall back to
/// the panel slot so a bad theme never drops a cell.
fn palette_fill(value: &str) -> Tok {
    let mut parts = value.split(';').map(|part| part.trim().parse::<u8>());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(r)), Some(Ok(g)), Some(Ok(b))) => Tok::Rgb(r, g, b),
        _ => Tok::Slot(S::Panel),
    }
}

fn swatch_strip(p: &Palette) -> [Tok; STRIP_CELLS] {
    [
        palette_fill(&p.bg0),
        palette_fill(&p.panel),
        palette_fill(&p.accent),
        palette_fill(&p.focus),
        palette_fill(&p.hues.green),
        palette_fill(&p.hues.red),
    ]
}

/// One preview segment and the palette role it demonstrates (`None` for
/// derived tokens that are not an editable role).
type PreviewSeg = (Option<ColorRole>, Seg);

fn pseg(role: Option<ColorRole>, fg: Tok, text: impl Into<String>) -> PreviewSeg {
    (role, seg(fg, text))
}

fn preview_segs() -> Vec<Vec<PreviewSeg>> {
    let dot = crate::caps::glyph(crate::caps::Glyph::DotFilled);
    let gap = || (None, sp(1));
    vec![
        vec![
            pseg(Some(ColorRole::Dim), Tok::Slot(S::Dim), format!("{dot} ")),
            pseg(Some(ColorRole::Text), Tok::Slot(S::Text), "sidebar row"),
            gap(),
            pseg(Some(ColorRole::Teal), Tok::Hue(Hue::Teal), "active"),
        ],
        vec![
            pseg(Some(ColorRole::Accent), Tok::Slot(S::Accent), " main "),
            gap(),
            pseg(Some(ColorRole::Focus), Tok::Slot(S::Focus), "focused tab"),
            gap(),
            pseg(Some(ColorRole::Purple), Tok::Hue(Hue::Purple), dot),
        ],
        vec![
            pseg(None, Tok::Slot(S::ActivityActive), " status "),
            gap(),
            pseg(Some(ColorRole::Green), Tok::Hue(Hue::Green), "ready"),
            gap(),
            pseg(Some(ColorRole::Amber), Tok::Hue(Hue::Amber), "warning"),
            gap(),
            pseg(Some(ColorRole::Red), Tok::Hue(Hue::Red), "error"),
        ],
        vec![
            pseg(Some(ColorRole::Green), Tok::Hue(Hue::Green), "+ added hunk"),
            gap(),
            pseg(Some(ColorRole::Red), Tok::Hue(Hue::Red), "- removed hunk"),
            gap(),
            pseg(Some(ColorRole::Blue), Tok::Sel(Hue::Blue, 35), "diff"),
        ],
        vec![
            pseg(Some(ColorRole::Border), Tok::Slot(S::Border), " pane "),
            gap(),
            pseg(Some(ColorRole::Text), Tok::Slot(S::Text), "structural text"),
            gap(),
            pseg(None, Tok::Slot(S::Ghost3), format!("{dot}{dot}{dot}")),
        ],
        vec![
            pseg(Some(ColorRole::Accent), Tok::SelAccent, " selected row "),
            gap(),
            pseg(None, Tok::Heat(4), "heat"),
            gap(),
            pseg(Some(ColorRole::Orange), Tok::Hue(Hue::Orange), "activity"),
        ],
    ]
}

/// Whether any preview segment demonstrates `role`.
fn preview_shows(role: ColorRole) -> bool {
    preview_segs()
        .iter()
        .flatten()
        .any(|(candidate, _)| *candidate == Some(role))
}

/// The legend under the preview: the elements `ELEMENT_GROUPS` says the role
/// paints, or a note that the mock does not show it.
fn preview_legend(role: ColorRole) -> String {
    if !preview_shows(role) {
        return "not shown in preview".into();
    }
    let mut labels: Vec<&str> = Vec::new();
    for (_, elements) in ELEMENT_GROUPS {
        for (label, element_role) in *elements {
            if *element_role == role && !labels.contains(label) {
                labels.push(label);
            }
        }
    }
    format!("highlighted: {}", labels.join(", "))
}

fn render_preview(surface: &mut Surface, preview: Rect, panel: Tok, highlight: Option<ColorRole>) {
    if preview.rows == 0 || preview.cols == 0 {
        return;
    }
    let marker = crate::caps::active_glyphs().arrow_right;
    for (row, segs) in preview_segs().into_iter().enumerate() {
        if row >= preview.rows {
            break;
        }
        let mut out = Vec::with_capacity(segs.len() + 1);
        let lit = highlight.is_some() && segs.iter().any(|(role, _)| *role == highlight);
        if lit {
            out.push(seg(Tok::Slot(S::Accent), marker));
        }
        for (role, s) in segs {
            if highlight.is_some() && role == highlight {
                out.push(s.under(seg::Under::Single));
            } else {
                out.push(s);
            }
        }
        seg::draw_line(
            surface,
            preview.x,
            preview.y + row,
            preview.cols,
            &Line::segs(out),
            panel,
        );
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
        for screen in [Rect::full(80, 24), Rect::full(106, 21), Rect::full(140, 40)] {
            let outer = box_rect(&layer_spec(), screen).expect("popup fits");
            let inner = Rect {
                x: outer.x + 2,
                y: outer.y + 1,
                cols: outer.cols - 4,
                rows: outer.rows - 2,
            };
            let layout = layout_for(inner);
            assert!(layout.apply.rows > 0);
            for rect in [layout.catalog, layout.editor, layout.preview, layout.apply] {
                assert!(rect.y + rect.rows < outer.y + outer.rows, "{screen:?}");
                assert!(rect.x + rect.cols < outer.x + outer.cols, "{screen:?}");
            }
            assert!(layout.editor.y + layout.editor.rows <= layout.apply.y);

            // Every editor mode, the picker included, renders inside the box
            // and keeps the selected role's contrast line visible.
            let mut b = builder();
            b.handle_key(&KeyCode::Char('v'), Modifiers::NONE);
            b.render_inside(screen, outer);
            b.handle_key(&KeyCode::Tab, Modifiers::NONE);
            b.handle_key(&KeyCode::Enter, Modifiers::NONE);
            assert!(b.picker.is_some());
            let text = b.render_inside(screen, outer);
            assert!(text.contains("ANSI 16"), "{screen:?}");
            assert!(text.contains("contrast"), "{screen:?}");
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
    fn every_role_highlights_a_preview_segment_or_says_it_is_not_shown() {
        for (role, _) in ROLES {
            let legend = preview_legend(role);
            if preview_shows(role) {
                assert!(legend.starts_with("highlighted: "), "{legend}");
                assert!(legend.len() > "highlighted: ".len(), "{legend}");
            } else {
                assert_eq!(legend, "not shown in preview");
            }
        }
    }

    #[test]
    fn selected_role_underlines_its_preview_segments_and_shows_a_legend() {
        let mut b = builder();
        // Token view, focus on the first role (bg0) is not in the mock; move
        // to `text`, which is.
        b.focus = ROLES
            .iter()
            .position(|(r, _)| *r == ColorRole::Text)
            .unwrap();
        let mut surface = Surface::new(106, 30);
        b.render(&mut surface, Rect::full(106, 30));
        let text = surface.screen_chars_to_string();
        assert!(text.contains("highlighted: "), "{text}");
        let underlined = surface
            .screen_cells()
            .into_iter()
            .flatten()
            .filter(|cell| cell.attrs().underline() != termwiz::cell::Underline::None)
            .count();
        assert!(underlined > 0);
    }

    #[test]
    fn swatch_strip_has_six_cells_and_drops_when_narrow() {
        let p = thegn_core::config::Config::default().palette();
        assert_eq!(swatch_strip(&p).len(), 6);
        assert_eq!(strip_width(CATALOG_COLS), 6);
        assert_eq!(strip_width(STRIP_MIN_COLS - 1), 0);
        assert_eq!(palette_fill("1;2;3"), Tok::Rgb(1, 2, 3));
        assert_eq!(palette_fill("nope"), Tok::Slot(S::Panel));
        for item in builder().catalog.iter() {
            assert_eq!(swatch_strip(&item.palette).len(), 6);
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
    fn catalog_row_keeps_kind_visible_for_long_names() {
        let mut cfg = thegn_core::config::Config::default();
        let long = "a-very-long-user-theme-name-30".to_string();
        assert_eq!(long.chars().count(), 30);
        let users = vec![UserTheme::from_palette(long.clone(), &cfg.palette())];
        cfg.theme.preset = long.clone();
        let b = ThemeBuilder::open(&cfg, &users);
        let mut surface = Surface::new(80, 24);
        b.render(&mut surface, Rect::full(80, 24));
        let text = surface.screen_chars_to_string();
        let row = text
            .lines()
            .find(|l| l.contains("a-very-long"))
            .expect("long theme row rendered");
        assert!(!row.contains(&long), "name should be clipped: {row:?}");
        assert!(row.contains("… user"), "ellipsis then kind: {row:?}");
    }

    #[test]
    fn catalog_row_short_name_is_unclipped() {
        let b = builder();
        let mut surface = Surface::new(80, 24);
        b.render(&mut surface, Rect::full(80, 24));
        let text = surface.screen_chars_to_string();
        let name = &b.catalog[0].name;
        let row = text.lines().find(|l| l.contains(name.as_str())).unwrap();
        assert!(row.contains("builtin"), "{row:?}");
        assert!(!row.contains('…'), "{row:?}");
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

    #[test]
    fn token_editor_ignores_non_hex_unicode_without_corrupting_its_byte_cursor() {
        let mut b = builder();
        let original = b.draft.colors.bg0.clone();
        b.focus = 0;
        assert_eq!(
            b.handle_key(&KeyCode::Enter, Modifiers::NONE),
            BuilderEvent::None
        );
        // Enter opens the picker; `#` switches it to the typed hex editor.
        assert!(b.picker.is_some());
        assert_eq!(
            b.handle_key(&KeyCode::Char('#'), Modifiers::NONE),
            BuilderEvent::None
        );
        assert!(b.picker.is_none());

        // A multi-byte character used to advance the cursor by one byte and
        // leave it inside the code point, so the following insertion panicked.
        assert_eq!(
            b.handle_key(&KeyCode::Char('é'), Modifiers::NONE),
            BuilderEvent::None
        );
        assert_eq!(
            b.handle_key(&KeyCode::Char('f'), Modifiers::NONE),
            BuilderEvent::None
        );
        assert_eq!(b.draft.colors.bg0, format!("{original}f"));
    }

    fn themed(name: &str, bg0: &str) -> UserTheme {
        let cfg = thegn_core::config::Config::default();
        let mut theme = UserTheme::from_palette(name, &cfg.palette());
        theme.colors.bg0 = bg0.to_string();
        theme
    }

    #[test]
    fn theme_collision_fixtures_agree_across_catalog_cycle_and_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let builtin_filename = format!(
            "{}.toml",
            thegn_core::util::slugify(thegn_core::theme::PRESETS[0])
        );
        for (filename, theme) in [
            ("aurora-borealis.toml", themed("Aurora Borealis", "#123456")),
            ("night-sky.toml", themed("Night-Sky", "#222222")),
            ("night_sky.toml", themed("night_sky", "#333333")),
            ("shared.toml", themed("Shared", "#444444")),
            ("shared-copy.toml", themed("Shared", "#555555")),
        ] {
            std::fs::write(dir.path().join(filename), theme.to_toml().unwrap()).unwrap();
        }
        std::fs::write(
            dir.path().join(builtin_filename),
            themed(thegn_core::theme::PRESETS[0], "#666666")
                .to_toml()
                .unwrap(),
        )
        .unwrap();

        let (users, warnings) = crate::theme_store::scan_dir(dir.path(), None);
        let fixture_names = ["Night-Sky", "night_sky", "Shared"];
        let user_names = users
            .iter()
            .map(|theme| theme.meta.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            user_names,
            ["Aurora Borealis"],
            "unexpected surviving fixture names: {user_names:?}; quarantined names: {fixture_names:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("ambiguous user theme identity `Shared`")),
            "missing exact-identity warning for fixture names {fixture_names:?}: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning
                    .contains("ambiguous normalized user theme identity `night-sky`")),
            "missing normalized-identity warning for fixture names {fixture_names:?}: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("conflicts with a built-in preset identity")),
            "missing built-in conflict warning for fixture names {fixture_names:?}: {warnings:?}"
        );

        let cfg = thegn_core::config::Config::default();
        let items = catalog(&cfg, &users);
        let catalog_names = items
            .iter()
            .map(|item| item.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            items.len(),
            thegn_core::theme::PRESETS.len() + 1,
            "unexpected catalog names for quarantined fixture names {fixture_names:?}: {catalog_names:?}"
        );
        let non_builtin_names = items
            .iter()
            .filter(|item| !item.builtin)
            .map(|item| item.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            non_builtin_names,
            ["Aurora Borealis".to_string()],
            "unexpected non-built-in catalog names for quarantined fixture names {fixture_names:?}: {catalog_names:?}"
        );
        assert!(
            fixture_names
                .iter()
                .all(|name| !catalog_names.iter().any(|item| item == name)),
            "quarantined fixture names appeared in catalog: {fixture_names:?}; catalog: {catalog_names:?}"
        );

        let cycle_names = cycle_catalog(&cfg, &users)
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert_eq!(
            cycle_names, catalog_names,
            "cycle/catalog name order differs for quarantined fixture names {fixture_names:?}"
        );

        let resolved = cfg.palette_with_user_themes("Aurora Borealis", &users);
        let unknown = cfg.palette_with_user_themes("definitely-not-a-theme", &users);
        assert_ne!(
            resolved.bg0, unknown.bg0,
            "valid fixture theme resolved like unknown; quarantined names: {fixture_names:?}"
        );
        assert_eq!(
            resolved.bg0,
            users[0].palette().unwrap().bg0,
            "valid fixture theme did not resolve to its own palette; quarantined names: {fixture_names:?}"
        );
        for name in fixture_names {
            assert_eq!(
                cfg.palette_with_user_themes(name, &users).bg0,
                unknown.bg0,
                "quarantined fixture name {name:?} did not resolve like unknown"
            );
        }
    }

    #[test]
    fn theme_collision_builtin_named_file_never_changes_the_builtin_palette() {
        let dir = tempfile::tempdir().unwrap();
        let builtin_name = thegn_core::theme::PRESETS[0];
        let filename = format!("{}.toml", thegn_core::util::slugify(builtin_name));
        std::fs::write(
            dir.path().join(filename),
            themed(builtin_name, "#666666").to_toml().unwrap(),
        )
        .unwrap();
        let colliding_theme =
            UserTheme::from_toml(&themed(builtin_name, "#666666").to_toml().unwrap()).unwrap();

        let (users, warnings) = crate::theme_store::scan_dir(dir.path(), None);
        assert!(
            users.is_empty(),
            "built-in collision fixture survived with names {:?}; warnings: {warnings:?}",
            users
                .iter()
                .map(|theme| theme.meta.name.as_str())
                .collect::<Vec<_>>()
        );
        let cfg = thegn_core::config::Config::default();
        assert_eq!(
            cfg.palette_with_user_themes(builtin_name, &[colliding_theme])
                .bg0,
            cfg.palette_with_preset(builtin_name).bg0,
            "built-in palette changed for collision fixture {builtin_name:?}"
        );
        let items = catalog(&cfg, &users);
        assert_eq!(items[0].name, builtin_name);
        assert!(
            items[0].builtin,
            "first catalog fixture {builtin_name:?} was not marked built-in"
        );
    }

    fn press(x: usize, y: usize) -> MouseEvent {
        MouseEvent {
            x: x as u16,
            y: y as u16,
            mouse_buttons: MouseButtons::LEFT,
            modifiers: Modifiers::NONE,
        }
    }

    fn wheel(down: bool) -> MouseEvent {
        MouseEvent {
            x: 0,
            y: 0,
            mouse_buttons: if down {
                MouseButtons::VERT_WHEEL
            } else {
                MouseButtons::VERT_WHEEL | MouseButtons::WHEEL_POSITIVE
            },
            modifiers: Modifiers::NONE,
        }
    }

    fn layout_at(screen: Rect) -> (Rect, BuilderLayout) {
        let outer = box_rect(&layer_spec(), screen).expect("popup fits");
        let inner = Rect {
            x: outer.x + 2,
            y: outer.y + 1,
            cols: outer.cols - 4,
            rows: outer.rows - 2,
        };
        (outer, layout_for(inner))
    }

    fn open_picker_on(b: &mut ThemeBuilder, role: ColorRole) {
        b.focus = ROLES.iter().position(|(r, _)| *r == role).unwrap();
        assert_eq!(
            b.handle_key(&KeyCode::Enter, Modifiers::NONE),
            BuilderEvent::None
        );
        assert_eq!(b.picker.as_ref().map(|state| state.role), Some(role));
    }

    #[test]
    fn picker_moves_preview_live_and_esc_restores_the_snapshot_exactly() {
        let mut b = builder();
        let original = role_value(&b.draft, ColorRole::Bg0);
        open_picker_on(&mut b, ColorRole::Bg0);
        assert_eq!(*b.candidate(), b.snapshot, "opening alone previews nothing");
        for key in [KeyCode::RightArrow, KeyCode::Char('j'), KeyCode::Char('l')] {
            let before = b.candidate().clone();
            b.handle_key(&key, Modifiers::NONE);
            assert_ne!(*b.candidate(), before, "{key:?} must preview live");
        }
        assert!(b.is_dirty());
        assert_eq!(b.edited, vec![ColorRole::Bg0]);
        assert_eq!(
            b.handle_key(&KeyCode::Escape, Modifiers::NONE),
            BuilderEvent::None,
            "Esc closes the picker, not the builder"
        );
        assert!(b.picker.is_none());
        assert_eq!(*b.candidate(), b.snapshot);
        assert_eq!(role_value(&b.draft, ColorRole::Bg0), original);
        assert!(b.edited.is_empty());
        assert!(!b.is_dirty());
    }

    #[test]
    fn picker_enter_keeps_like_a_typed_edit_until_apply() {
        let mut b = builder();
        open_picker_on(&mut b, ColorRole::Accent);
        b.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
        let kept = role_value(&b.draft, ColorRole::Accent);
        b.handle_key(&KeyCode::Enter, Modifiers::NONE);
        assert!(b.picker.is_none());
        assert_eq!(b.edited, vec![ColorRole::Accent]);
        assert_eq!(
            b.edited_overrides().and_then(|o| o.accent),
            Some(kept.clone())
        );
        assert!(b.is_dirty());
        // Builder Esc still discards the transient edit, exactly as a typed
        // hex edit: the handler reinstalls the snapshot palette.
        assert_eq!(
            b.handle_key(&KeyCode::Escape, Modifiers::NONE),
            BuilderEvent::Close
        );
        assert_eq!(b.cancel_palette(), b.snapshot);
        // A second picker session on a kept role reverts to the kept value
        // and leaves the role an intentional edit.
        let mut b2 = builder();
        open_picker_on(&mut b2, ColorRole::Accent);
        b2.handle_key(&KeyCode::RightArrow, Modifiers::NONE);
        b2.handle_key(&KeyCode::Enter, Modifiers::NONE);
        let kept = role_value(&b2.draft, ColorRole::Accent);
        b2.handle_key(&KeyCode::Enter, Modifiers::NONE);
        b2.handle_key(&KeyCode::RightArrow, Modifiers::NONE);
        b2.handle_key(&KeyCode::Escape, Modifiers::NONE);
        assert_eq!(role_value(&b2.draft, ColorRole::Accent), kept);
        assert_eq!(b2.edited, vec![ColorRole::Accent]);
    }

    #[test]
    fn picker_hash_switches_to_hex_and_its_esc_restores_the_pre_picker_value() {
        let mut b = builder();
        let original = role_value(&b.draft, ColorRole::Text);
        open_picker_on(&mut b, ColorRole::Text);
        b.handle_key(&KeyCode::RightArrow, Modifiers::NONE);
        b.handle_key(&KeyCode::Char('#'), Modifiers::NONE);
        assert!(b.picker.is_none());
        assert!(matches!(b.editing, Some((ColorRole::Text, ..))));
        b.handle_key(&KeyCode::Escape, Modifiers::NONE);
        assert_eq!(role_value(&b.draft, ColorRole::Text), original);
        assert!(b.edited.is_empty());
        assert_eq!(*b.candidate(), b.snapshot);
    }

    #[test]
    fn v_toggles_the_element_view_and_groups_collapse() {
        let mut b = builder();
        b.handle_key(&KeyCode::Char('v'), Modifiers::NONE);
        assert_eq!(b.view, EditorView::Elements);
        let full = element_rows(&b.collapsed).len();
        // Cursor starts on the first group header: Enter collapses it.
        b.handle_key(&KeyCode::Enter, Modifiers::NONE);
        assert_eq!(
            element_rows(&b.collapsed).len(),
            full - ELEMENT_GROUPS[0].1.len()
        );
        b.handle_key(&KeyCode::RightArrow, Modifiers::NONE);
        assert_eq!(element_rows(&b.collapsed).len(), full);
        b.handle_key(&KeyCode::LeftArrow, Modifiers::NONE);
        b.handle_key(&KeyCode::Char(' '), Modifiers::NONE);
        assert_eq!(element_rows(&b.collapsed).len(), full);
        // Tab to the first element, Enter opens the picker for its role.
        b.handle_key(&KeyCode::Tab, Modifiers::NONE);
        b.handle_key(&KeyCode::Enter, Modifiers::NONE);
        assert_eq!(
            b.picker.as_ref().map(|state| state.role),
            Some(ELEMENT_GROUPS[0].1[0].1)
        );
        assert_eq!(b.selected_role(), Some(ELEMENT_GROUPS[0].1[0].1));
        b.handle_key(&KeyCode::Escape, Modifiers::NONE);
        assert!(b.picker.is_none());
        b.handle_key(&KeyCode::Char('v'), Modifiers::NONE);
        assert_eq!(b.view, EditorView::Tokens);
        b.handle_key(&KeyCode::Char('v'), Modifiers::NONE);
        assert_eq!(b.view, EditorView::Elements);
        // Shift-Tab from the top wraps to the Apply row.
        b.elem_cursor = 0;
        b.handle_key(&KeyCode::Tab, Modifiers::SHIFT);
        assert!(b.apply_focused());
        assert!(matches!(
            b.handle_key(&KeyCode::Enter, Modifiers::NONE),
            BuilderEvent::Apply { .. }
        ));
    }

    #[test]
    fn wheel_scrolls_the_element_list_and_the_catalog_without_selecting() {
        let screen = Rect::full(80, 24);
        let (_, layout) = layout_at(screen);
        let mut b = builder();
        b.handle_key(&KeyCode::Char('v'), Modifiers::NONE);
        let mut left_down = false;
        let (ex, ey) = (layout.editor.x + 3, layout.editor.y + 1);
        assert_eq!(b.elements_start(element_rows(&b.collapsed).len(), 20), 0);
        for _ in 0..3 {
            b.handle_mouse(&wheel(true), ex, ey, screen, true, &mut left_down);
        }
        assert_eq!(b.elem_scroll, Some(3));
        let text = b.render_inside(screen, box_rect(&layer_spec(), screen).unwrap());
        assert!(
            !text.contains(ELEMENT_GROUPS[0].0),
            "first group scrolled off"
        );
        b.handle_mouse(&wheel(false), ex, ey, screen, true, &mut left_down);
        assert_eq!(b.elem_scroll, Some(2));
        assert_eq!(b.elem_cursor, 0, "the wheel scrolls, it does not move");

        let selected = b.selected;
        let (cx, cy) = (layout.catalog.x + 1, layout.catalog.y + 1);
        b.handle_mouse(&wheel(true), cx, cy, screen, true, &mut left_down);
        assert_eq!(b.selected, selected);
        // A clicked catalog row maps through the scrolled offset.
        let start = list_start(
            b.catalog_scroll,
            b.selected,
            layout.catalog.rows,
            b.catalog.len(),
        );
        b.handle_mouse(&press(cx, cy), cx, cy, screen, true, &mut left_down);
        assert_eq!(b.selected, (start + 1).min(b.catalog.len() - 1));
    }

    #[test]
    fn clicking_a_picker_cell_previews_then_keeps_and_outside_rules_hold() {
        let screen = Rect::full(106, 30);
        let (outer, layout) = layout_at(screen);
        let mut b = builder();
        open_picker_on(&mut b, ColorRole::Bg0);
        let mut left_down = false;
        // Line 0 is the title, line 1 the "theme" header, line 2 its first
        // row of cells; the second cell sits one cell-width right.
        let x = layout.editor.x + picker_cell_width(layout.editor.cols);
        let y = layout.editor.y + 2;
        assert_eq!(b.picker_hit(layout.editor, x, y), Some((0, 1)));
        b.handle_mouse(&press(x, y), x, y, screen, true, &mut left_down);
        left_down = false;
        assert!(b.picker.is_some());
        assert_eq!(
            role_value(&b.draft, ColorRole::Bg0),
            b.picker_cells[0].cells[1]
        );
        assert!(b.is_dirty());
        b.handle_mouse(&press(x, y), x, y, screen, true, &mut left_down);
        left_down = false;
        assert!(b.picker.is_none(), "a second click keeps");
        assert_eq!(b.edited, vec![ColorRole::Bg0]);

        // Dirty: an outside click must not dismiss; clean: it does.
        let (ox, oy) = (outer.x.saturating_sub(1), outer.y.saturating_sub(1));
        assert_eq!(
            b.handle_mouse(&press(ox, oy), ox, oy, screen, true, &mut left_down),
            BuilderEvent::None
        );
        left_down = false;
        let mut clean = builder();
        assert_eq!(
            clean.handle_mouse(&press(ox, oy), ox, oy, screen, true, &mut left_down),
            BuilderEvent::Close
        );
        left_down = false;
        assert_eq!(
            clean.handle_mouse(&press(ox, oy), ox, oy, screen, false, &mut left_down),
            BuilderEvent::None
        );
    }

    #[test]
    fn clicking_an_element_swatch_opens_the_picker_and_a_label_only_focuses() {
        let screen = Rect::full(106, 30);
        let (_, layout) = layout_at(screen);
        let mut b = builder();
        b.handle_key(&KeyCode::Char('v'), Modifiers::NONE);
        let mut left_down = false;
        let y = layout.editor.y + 1; // first element of the first group
        let label_x = layout.editor.x + 8;
        b.handle_mouse(&press(label_x, y), label_x, y, screen, true, &mut left_down);
        left_down = false;
        assert_eq!(b.elem_cursor, 1);
        assert!(b.picker.is_none());
        let swatch_x = layout.editor.x + 2;
        b.handle_mouse(
            &press(swatch_x, y),
            swatch_x,
            y,
            screen,
            true,
            &mut left_down,
        );
        assert_eq!(
            b.picker.as_ref().map(|state| state.role),
            Some(ELEMENT_GROUPS[0].1[0].1)
        );
    }

    #[test]
    fn contrast_line_names_a_failing_pair_for_the_selected_role() {
        let mut b = builder();
        let bg0 = role_value(&b.draft, ColorRole::Bg0);
        b.set_role_for_test(ColorRole::Text, &bg0);
        remember_edit(&mut b.edited, ColorRole::Text);
        b.refresh_candidate();
        b.focus = ROLES
            .iter()
            .position(|(role, _)| *role == ColorRole::Text)
            .unwrap();
        let screen = Rect::full(80, 24);
        let text = b.render_inside(screen, box_rect(&layer_spec(), screen).unwrap());
        assert!(text.contains("text contrast 1.0:1"), "{text}");
        assert!(text.contains("below"), "{text}");
    }

    impl ThemeBuilder {
        fn input_value_for_test(&self) -> Option<&str> {
            self.input.as_ref().map(|(_, value)| value.as_str())
        }

        fn set_role_for_test(&mut self, role: ColorRole, value: &str) {
            set_role(&mut self.draft, role, value.into());
        }

        /// Render at `screen`, assert nothing paints outside `outer`, and
        /// return the screen text.
        fn render_inside(&self, screen: Rect, outer: Rect) -> String {
            let mut surface = Surface::new(screen.cols, screen.rows);
            self.render(&mut surface, screen);
            for (y, row) in surface.screen_cells().iter().enumerate() {
                for (x, cell) in row.iter().enumerate() {
                    if !outer.contains(x, y) {
                        assert_eq!(cell.str(), " ", "painted outside at {x},{y}");
                    }
                }
            }
            surface.screen_chars_to_string()
        }
    }
}
