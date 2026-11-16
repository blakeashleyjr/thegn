//! The editable palette roles and the UI elements each one paints (THE-2023).
//!
//! Pure data: the builder's element view renders [`ELEMENT_GROUPS`] so a user
//! edits "the sidebar selection" rather than guessing which of the twenty
//! palette tokens that is. Every pair below names a draw site that really
//! uses the role today (the file in the trailing comment); a role may appear
//! in several groups, and every role appears at least once.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ColorRole {
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

/// Every editable role in token-view order, with its config/theme-file name.
pub(super) const ROLES: [(ColorRole, &str); 20] = [
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

/// The config name of a role (`"bg0"`, `"amber"`, …).
pub(super) fn role_name(role: ColorRole) -> &'static str {
    ROLES
        .iter()
        .find(|(candidate, _)| *candidate == role)
        .map_or("", |(_, name)| name)
}

/// One element row: the user-facing label and the role that paints it.
pub(super) type Element = (&'static str, ColorRole);

/// One collapsible group of the element view.
pub(super) type ElementGroup = (&'static str, &'static [Element]);

pub(super) const ELEMENT_GROUPS: &[ElementGroup] = &[
    (
        "Sidebar",
        &[
            // sidebar_view.rs: row labels, secondary text, the focus marker,
            // the bg0 header band, the panel rail, selection and hover fills.
            ("rows", ColorRole::Text),
            ("dim rows", ColorRole::Dim),
            ("muted labels", ColorRole::Faint),
            ("selection", ColorRole::Focus),
            ("header band", ColorRole::Bg0),
            ("rail", ColorRole::Panel),
            ("selected row", ColorRole::Panel2),
            ("hover", ColorRole::Raise),
            ("separators", ColorRole::Border),
            ("accent marks", ColorRole::Accent),
        ],
    ),
    (
        "Tabs + Masthead",
        &[
            // chrome.rs draw_masthead + masthead.rs.
            ("bar background", ColorRole::Panel),
            ("focused bar", ColorRole::Raise),
            ("title", ColorRole::Text),
            ("hints", ColorRole::Dim),
            ("separators", ColorRole::Ghost),
            ("focus mark", ColorRole::Focus),
        ],
    ),
    (
        "Status bar",
        &[
            // chrome.rs draw_statusbar/statusbar_items + statusbar_*.rs.
            ("background", ColorRole::Panel),
            ("text", ColorRole::Dim),
            ("alert", ColorRole::Red),
            ("warning", ColorRole::Amber),
            ("server chip", ColorRole::Blue),
            ("persist chip", ColorRole::Teal),
            ("client chip", ColorRole::Purple),
        ],
    ),
    (
        "Panes",
        &[
            // chrome.rs render_panes / draw_columns_frame / draw_plugin_view.
            ("border", ColorRole::Border),
            ("focused border", ColorRole::Focus),
            ("plugin view", ColorRole::Bg1),
            ("title", ColorRole::Text),
        ],
    ),
    (
        "Diff",
        &[
            // diff_view.rs.
            ("added", ColorRole::Green),
            ("removed", ColorRole::Red),
            ("notes", ColorRole::Amber),
            ("path", ColorRole::Text),
            ("context", ColorRole::Dim),
            ("markers", ColorRole::Faint),
        ],
    ),
    (
        "Activity dots",
        &[
            // theme.rs derives activity_waiting/done from red/amber;
            // sidebar_view.rs paints them.
            ("blocked", ColorRole::Red),
            ("finished", ColorRole::Amber),
            ("idle", ColorRole::Ghost),
        ],
    ),
    (
        "Git graph",
        &[
            // panel/graph.rs lane hues.
            ("lane teal", ColorRole::Teal),
            ("lane magenta", ColorRole::Magenta),
            ("lane purple", ColorRole::Purple),
            ("lane orange", ColorRole::Orange),
        ],
    ),
    (
        "Popups + modals",
        &[
            // layer.rs + every LayerSpec (this builder's own frame included).
            ("background", ColorRole::Panel),
            ("border", ColorRole::Focus),
            ("raised", ColorRole::Raise),
            ("accent border", ColorRole::Accent),
        ],
    ),
];

/// A visible row of the element view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ElementRow {
    Group(usize),
    Element(usize, usize),
}

/// The visible rows given each group's collapsed flag (missing = expanded).
pub(super) fn element_rows(collapsed: &[bool]) -> Vec<ElementRow> {
    let mut rows = Vec::new();
    for (group, (_, elements)) in ELEMENT_GROUPS.iter().enumerate() {
        rows.push(ElementRow::Group(group));
        if !collapsed.get(group).copied().unwrap_or(false) {
            rows.extend((0..elements.len()).map(|element| ElementRow::Element(group, element)));
        }
    }
    rows
}

/// The role an element row edits (`None` for a group header).
pub(super) fn row_role(row: ElementRow) -> Option<ColorRole> {
    match row {
        ElementRow::Group(_) => None,
        ElementRow::Element(group, element) => ELEMENT_GROUPS
            .get(group)
            .and_then(|(_, elements)| elements.get(element))
            .map(|(_, role)| *role),
    }
}

#[cfg(test)]
mod tests {
    use super::{ELEMENT_GROUPS, ElementRow, ROLES, element_rows, role_name, row_role};

    #[test]
    fn every_color_role_paints_at_least_one_element() {
        for (role, name) in ROLES {
            assert!(
                ELEMENT_GROUPS
                    .iter()
                    .any(|(_, elements)| elements.iter().any(|(_, r)| *r == role)),
                "role {name} is not listed in any element group"
            );
            assert_eq!(role_name(role), name);
        }
    }

    #[test]
    fn group_and_element_labels_are_unique() {
        let mut groups = ELEMENT_GROUPS
            .iter()
            .map(|(group, _)| *group)
            .collect::<Vec<_>>();
        groups.sort_unstable();
        groups.dedup();
        assert_eq!(groups.len(), ELEMENT_GROUPS.len());
        for (group, elements) in ELEMENT_GROUPS {
            let mut labels = elements.iter().map(|(label, _)| *label).collect::<Vec<_>>();
            labels.sort_unstable();
            labels.dedup();
            assert_eq!(labels.len(), elements.len(), "duplicate label in {group}");
        }
    }

    #[test]
    fn collapsing_a_group_hides_only_its_elements() {
        let all = element_rows(&[]);
        let total = ELEMENT_GROUPS.len()
            + ELEMENT_GROUPS
                .iter()
                .map(|(_, elements)| elements.len())
                .sum::<usize>();
        assert_eq!(all.len(), total);
        let collapsed = element_rows(&[true]);
        assert_eq!(collapsed.len(), total - ELEMENT_GROUPS[0].1.len());
        assert_eq!(collapsed[0], ElementRow::Group(0));
        assert_eq!(collapsed[1], ElementRow::Group(1));
        assert_eq!(row_role(ElementRow::Group(0)), None);
        assert_eq!(
            row_role(ElementRow::Element(0, 0)),
            Some(ELEMENT_GROUPS[0].1[0].1)
        );
    }
}
