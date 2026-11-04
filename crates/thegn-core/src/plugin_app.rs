//! Versioned, renderer-independent app widgets. No terminal escape sequences.
use crate::plugin_api::Span;
use serde::{Deserialize, Serialize};

mod events;
pub use events::*;

#[cfg(test)]
mod validation_tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AppView {
    #[serde(default)]
    pub state_scope: String,
    pub revision: u64,
    pub root: Widget,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Item {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub depth: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Field {
    #[serde(default)]
    pub kind: FieldKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub helper: String,
    #[serde(default)]
    pub error: String,
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub multiline: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<ChoiceOption>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Action {
    #[serde(default)]
    pub binding: InteractionBinding,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub disabled_reason: String,
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Widget {
    Split {
        id: String,
        children: Vec<Widget>,
        #[serde(default)]
        vertical: bool,
    },
    List {
        id: String,
        title: String,
        items: Vec<Item>,
    },
    Text {
        id: String,
        title: String,
        text: String,
    },
    Form {
        id: String,
        #[serde(default)]
        instance: String,
        #[serde(default)]
        binding: InteractionBinding,
        title: String,
        fields: Vec<Field>,
        submit: String,
    },
    Actions {
        id: String,
        actions: Vec<Action>,
    },
    Confirm {
        id: String,
        #[serde(default)]
        binding: InteractionBinding,
        message: String,
        accept: String,
    },
    WeightedSplit {
        id: String,
        children: Vec<SplitChild>,
        #[serde(default)]
        vertical: bool,
    },
    Table {
        id: String,
        title: String,
        columns: Vec<TableColumn>,
        rows: Vec<TableRow>,
        #[serde(default)]
        multiple: bool,
        #[serde(default)]
        actions: Vec<Action>,
    },
    Tree {
        id: String,
        title: String,
        items: Vec<TreeItem>,
        #[serde(default)]
        multiple: bool,
        #[serde(default)]
        actions: Vec<Action>,
    },
    Progress {
        id: String,
        label: String,
        current: Option<u64>,
        total: Option<u64>,
        cancel: Option<Action>,
    },
    Presentation {
        id: String,
        title: String,
        message: String,
        state: PresentationState,
        #[serde(default)]
        actions: Vec<Action>,
    },
    Grid {
        id: String,
        width: u16,
        height: u16,
        cells: Vec<GridCell>,
        cursor: Option<(u16, u16)>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    #[default]
    Text,
    Choice,
    Toggle,
    Integer,
    Date,
    DateTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
    /// Raw form value; routing identity remains separate from stored content.
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SplitChild {
    pub widget: Box<Widget>,
    pub weight: u16,
    pub min_size: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TableColumn {
    pub id: String,
    pub label: String,
    pub width: Option<u16>,
    #[serde(default)]
    pub sortable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TableRow {
    pub id: String,
    pub cells: Vec<String>,
    #[serde(default)]
    pub binding: InteractionBinding,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TreeItem {
    pub id: String,
    pub label: String,
    pub parent: Option<String>,
    #[serde(default)]
    pub binding: InteractionBinding,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresentationState {
    Loading,
    Empty,
    Offline,
    Stale,
    Error,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GridCell {
    pub x: u16,
    pub y: u16,
    pub span: Span,
}

impl Widget {
    pub fn id(&self) -> &str {
        match self {
            Self::Split { id, .. }
            | Self::List { id, .. }
            | Self::Text { id, .. }
            | Self::Form { id, .. }
            | Self::Actions { id, .. }
            | Self::Confirm { id, .. }
            | Self::Grid { id, .. }
            | Self::WeightedSplit { id, .. }
            | Self::Table { id, .. }
            | Self::Tree { id, .. }
            | Self::Progress { id, .. }
            | Self::Presentation { id, .. } => id,
        }
    }
    /// Traverse both legacy and constrained splits without flattening ownership.
    pub fn child_widgets(&self) -> impl Iterator<Item = &Widget> {
        let weighted = match self {
            Self::WeightedSplit { children, .. } => children.as_slice(),
            _ => &[],
        };
        self.children()
            .iter()
            .chain(weighted.iter().map(|c| c.widget.as_ref()))
    }
    pub fn children(&self) -> &[Widget] {
        match self {
            Self::Split { children, .. } => children,
            _ => &[],
        }
    }
}

impl AppView {
    /// Bound the work performed by the synchronous host renderer. The transport
    /// separately caps encoded messages at one MiB.
    pub fn validate(&self) -> Result<(), String> {
        if !safe_text(&self.status)
            || self.state_scope.len() > 256
            || self.state_scope.chars().any(char::is_control)
        {
            return Err("invalid app state scope".into());
        }
        let mut ids = std::collections::HashSet::new();
        let mut stack = vec![(&self.root, 0)];
        let mut total = 0;
        let mut entries = 0usize;
        while let Some((w, depth)) = stack.pop() {
            total += 1;
            if depth > 12 || total > 2048 {
                return Err("app widget budget exceeded".into());
            }
            if !valid_id(w.id()) || !ids.insert(w.id()) {
                return Err("app widget IDs must be nonempty and unique".into());
            }
            entries = entries.saturating_add(match w {
                Widget::List { items, .. } => items.len(),
                Widget::Table {
                    columns,
                    rows,
                    actions,
                    ..
                } => columns
                    .len()
                    .saturating_mul(rows.len())
                    .saturating_add(actions.len()),
                Widget::Tree { items, actions, .. } => items.len().saturating_add(actions.len()),
                Widget::Form { fields, .. } => fields
                    .iter()
                    .map(|f| f.choices.len().saturating_add(1))
                    .sum(),
                Widget::Grid { cells, .. } => cells.len(),
                Widget::Actions { actions, .. } | Widget::Presentation { actions, .. } => {
                    actions.len()
                }
                _ => 0,
            });
            if entries > 65536 {
                return Err("aggregate app content budget exceeded".into());
            }
            let safe = match w {
                Widget::List { title, items, .. } => {
                    safe_text(title) && items.iter().all(|i| safe_text(&i.label))
                }
                Widget::Text { title, text, .. } => safe_text(title) && safe_text(text),
                Widget::Form {
                    title,
                    fields,
                    submit,
                    ..
                } => {
                    safe_text(title)
                        && safe_text(submit)
                        && fields.iter().all(|f| {
                            safe_text(&f.label)
                                && safe_text(&f.value)
                                && safe_text(&f.helper)
                                && safe_text(&f.error)
                                && f.choices
                                    .iter()
                                    .all(|c| safe_text(&c.label) && safe_text(&c.value))
                        })
                }
                Widget::Table {
                    title,
                    columns,
                    rows,
                    ..
                } => {
                    safe_text(title)
                        && columns.iter().all(|c| safe_text(&c.label))
                        && rows.iter().all(|r| r.cells.iter().all(|c| safe_text(c)))
                }
                Widget::Tree { title, items, .. } => {
                    safe_text(title) && items.iter().all(|i| safe_text(&i.label))
                }
                Widget::Confirm {
                    message, accept, ..
                } => safe_text(message) && safe_text(accept),
                Widget::Progress { label, .. } => safe_text(label),
                Widget::Presentation { title, message, .. } => {
                    safe_text(title) && safe_text(message)
                }
                _ => true,
            };
            if !safe {
                return Err("app text contains terminal controls".into());
            }
            match w {
                Widget::List { items, .. } => {
                    let mut seen = std::collections::HashSet::new();
                    if items.len() > 4096
                        || items
                            .iter()
                            .any(|i| !valid_id(&i.id) || !seen.insert(&i.id))
                    {
                        return Err("invalid list items".into());
                    }
                }
                Widget::Form {
                    fields,
                    instance,
                    binding,
                    ..
                } => {
                    let mut seen = std::collections::HashSet::new();
                    if instance.len() > 256
                        || instance.chars().any(char::is_control)
                        || !valid_binding(binding)
                        || fields.len() > 64
                        || fields.iter().any(|f| {
                            let mut options = std::collections::HashSet::new();
                            !valid_id(&f.id)
                                || !seen.insert(&f.id)
                                || f.value.len() > 32 * 1024
                                || f.choices.len() > 4096
                                || f.choices.iter().any(|o| {
                                    !valid_id(&o.id)
                                        || !options.insert(&o.id)
                                        || o.value.len() > 32 * 1024
                                })
                        })
                    {
                        return Err("invalid form fields or binding".into());
                    }
                }
                Widget::WeightedSplit { children, .. } => {
                    if children.is_empty()
                        || children.len() > 64
                        || children.iter().any(|c| c.weight == 0 || c.min_size > 4096)
                    {
                        return Err("invalid split constraints".into());
                    }
                }
                Widget::Table {
                    columns,
                    rows,
                    actions,
                    ..
                } => {
                    let mut column_ids = std::collections::HashSet::new();
                    let mut row_ids = std::collections::HashSet::new();
                    if columns.is_empty()
                        || columns.len() > 64
                        || rows.len() > 4096
                        || columns.iter().any(|c| {
                            !valid_id(&c.id) || !column_ids.insert(&c.id) || c.width == Some(0)
                        })
                        || rows.iter().any(|r| {
                            !valid_id(&r.id)
                                || !row_ids.insert(&r.id)
                                || r.cells.len() != columns.len()
                                || !valid_binding(&r.binding)
                        })
                        || !valid_actions(actions)
                    {
                        return Err("invalid table".into());
                    }
                }
                Widget::Tree { items, actions, .. } => {
                    let mut nodes = std::collections::HashMap::new();
                    if items.len() > 4096
                        || !valid_actions(actions)
                        || items.iter().any(|i| {
                            !valid_id(&i.id)
                                || !valid_binding(&i.binding)
                                || nodes.insert(i.id.as_str(), i.parent.as_deref()).is_some()
                        })
                    {
                        return Err("invalid tree items".into());
                    }
                    for item in items {
                        let mut parent = item.parent.as_deref();
                        let mut seen = std::collections::HashSet::from([item.id.as_str()]);
                        while let Some(id) = parent {
                            if !seen.insert(id) || seen.len() > 128 {
                                return Err("tree cycle or depth exceeded".into());
                            }
                            parent = *nodes.get(id).ok_or("tree references missing parent")?;
                        }
                    }
                }
                Widget::Actions { actions, .. } | Widget::Presentation { actions, .. } => {
                    if !valid_actions(actions) {
                        return Err("invalid actions".into());
                    }
                }
                Widget::Confirm {
                    binding, accept, ..
                } => {
                    if !valid_binding(binding) || !valid_id(accept) {
                        return Err("invalid confirmation action or binding".into());
                    }
                }
                Widget::Progress {
                    current,
                    total,
                    cancel,
                    ..
                } => {
                    if current.zip(*total).is_some_and(|(n, total)| n > total)
                        || cancel
                            .as_ref()
                            .is_some_and(|a| !valid_actions(std::slice::from_ref(a)))
                    {
                        return Err("invalid progress".into());
                    }
                }
                Widget::Grid {
                    width,
                    height,
                    cells,
                    cursor,
                    ..
                } => {
                    let mut seen = std::collections::HashSet::new();
                    if *width > 512
                        || *height > 256
                        || cells.len() > 32768
                        || cursor.is_some_and(|(x, y)| x >= *width || y >= *height)
                        || cells.iter().any(|c| {
                            c.x >= *width
                                || c.y >= *height
                                || c.span.text.is_empty()
                                || c.span.text.chars().any(char::is_control)
                                || {
                                    let display =
                                        unicode_width::UnicodeWidthStr::width(c.span.text.as_str());
                                    display == 0
                                        || display > 2
                                        || c.x as usize + display > *width as usize
                                        || (0..display)
                                            .any(|offset| !seen.insert((c.x + offset as u16, c.y)))
                                }
                        })
                    {
                        return Err("invalid grid frame".into());
                    }
                }
                _ => {}
            }
            stack.extend(w.children().iter().map(|c| (c, depth + 1)));
            if let Widget::WeightedSplit { children, .. } = w {
                stack.extend(children.iter().map(|c| (c.widget.as_ref(), depth + 1)));
            }
        }
        Ok(())
    }
}

pub(super) fn safe_text(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}
fn valid_binding(binding: &InteractionBinding) -> bool {
    binding.target.len() <= 16
        && binding
            .target
            .iter()
            .all(|(k, v)| valid_id(k) && v.len() <= 1024 && !v.chars().any(char::is_control))
        && binding
            .precondition
            .as_ref()
            .is_none_or(|p| p.len() <= 4096 && !p.chars().any(char::is_control))
}
fn valid_actions(actions: &[Action]) -> bool {
    let mut ids = std::collections::HashSet::new();
    actions.len() <= 128
        && actions.iter().all(|a| {
            valid_id(&a.id)
                && ids.insert(&a.id)
                && valid_binding(&a.binding)
                && safe_text(&a.label)
                && safe_text(&a.disabled_reason)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_duplicate_widget_and_item_ids() {
        let list = Widget::List {
            id: "list".into(),
            title: "".into(),
            items: vec![],
        };
        let view = AppView {
            state_scope: String::new(),
            revision: 1,
            status: "".into(),
            root: Widget::Split {
                id: "root".into(),
                children: vec![list.clone(), list],
                vertical: false,
            },
        };
        assert!(view.validate().is_err());
    }
    #[test]
    fn bounded_grid_roundtrips() {
        let view = AppView {
            state_scope: String::new(),
            revision: 1,
            status: "".into(),
            root: Widget::Grid {
                id: "grid".into(),
                width: 80,
                height: 24,
                cells: vec![],
                cursor: Some((0, 0)),
            },
        };
        assert!(view.validate().is_ok());
        assert_eq!(
            serde_json::from_str::<AppView>(&serde_json::to_string(&view).unwrap()).unwrap(),
            view
        );
    }
}
