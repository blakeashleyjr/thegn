//! Pure classification of plugin statusbar changes.
//!
//! Plugin messages are coalesced before the compositor chooses a frame.  This
//! module keeps the policy for deciding whether the resulting statusbar view
//! set is unchanged, content-only, or structural out of the event loop.

use thegn_core::plugin_api::View;

/// The narrowest render damage caused by a batch of plugin messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PluginDamage {
    /// No visible statusbar change: the render loop can remain idle.
    None,
    /// Existing statusbar segments changed without changing their rendered
    /// widths or placement.  Only the statusbar row needs repainting.
    StatusbarContent,
    /// A contribution/lifecycle or layout change.  The chrome compositor must
    /// recompute statusbar fitting and placement in a full frame.
    Structural,
}

impl PluginDamage {
    pub(crate) fn merge(&mut self, other: Self) {
        *self = (*self).max(other);
    }
}

/// Width occupied by the view as the statusbar painter renders it.
///
/// The statusbar is single-row today, so it paints `View::spans` (the v0.2
/// compatibility row even when a newer plugin also supplies `rows`).  Measure
/// display cells, not Rust scalar values: a wide glyph or a combining mark is
/// exactly where a character-count comparison can falsely call a layout change
/// content-only.
pub(crate) fn rendered_width(view: &View) -> usize {
    view.spans
        .iter()
        .map(|span| crate::seg::cells(&span.text))
        .sum()
}

/// Classify the visible statusbar view set before and after one drained batch.
///
/// Equal ordered labels and rendered widths imply equal placement because the
/// statusbar lays plugin segments out in that stable order between unchanged
/// left and right clusters.  Any uncertainty — including an added/removed
/// segment, an order change, or a width change in either direction — escalates
/// to [`PluginDamage::Structural`].
pub(crate) fn classify(before: &[(String, View)], after: &[(String, View)]) -> PluginDamage {
    if before == after {
        return PluginDamage::None;
    }
    if before.len() == after.len()
        && before.iter().zip(after).all(
            |((before_label, before_view), (after_label, after_view))| {
                before_label == after_label
                    && rendered_width(before_view) == rendered_width(after_view)
            },
        )
    {
        PluginDamage::StatusbarContent
    } else {
        PluginDamage::Structural
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::surface::{Change, Position, Surface};
    use thegn_core::plugin_api::{Span, StyleRole};

    fn view(text: &str, role: StyleRole) -> View {
        View::line([Span::styled(text, role)])
    }

    fn change_stats(changes: &[Change]) -> (usize, usize) {
        let rows = changes
            .iter()
            .filter_map(|change| match change {
                Change::CursorPosition {
                    y: Position::Absolute(y),
                    ..
                } => Some(*y),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let bytes = changes
            .iter()
            .filter_map(|change| match change {
                Change::Text(text) => Some(text.len()),
                _ => None,
            })
            .sum();
        (rows.len(), bytes)
    }

    fn diff_bytes(front: &Surface, next: &Surface, row: usize, cols: usize) -> (usize, usize) {
        change_stats(&front.diff_region(0, row, cols, 1, next, 0, row))
    }

    #[test]
    fn identical_view_is_no_damage() {
        let v = view("ready", StyleRole::Default);
        assert_eq!(
            classify(&[("p".into(), v.clone())], &[("p".into(), v)]),
            PluginDamage::None
        );
    }

    #[test]
    fn same_width_content_change_is_statusbar_only() {
        assert_eq!(
            classify(
                &[("p".into(), view("ready", StyleRole::Default))],
                &[("p".into(), view("error", StyleRole::Error))],
            ),
            PluginDamage::StatusbarContent
        );
    }

    #[test]
    fn rendered_width_uses_terminal_cells_not_scalar_count() {
        assert_eq!(rendered_width(&view("界", StyleRole::Default)), 2);
        assert_eq!(
            classify(
                &[("p".into(), view("界", StyleRole::Default))],
                &[("p".into(), view("ab", StyleRole::Default))],
            ),
            PluginDamage::StatusbarContent
        );
    }

    #[test]
    fn both_width_change_directions_are_structural() {
        for (before, after) in [("ready", "longer"), ("longer", "ready")] {
            assert_eq!(
                classify(
                    &[("p".into(), view(before, StyleRole::Default))],
                    &[("p".into(), view(after, StyleRole::Default))],
                ),
                PluginDamage::Structural,
                "{before:?} -> {after:?}"
            );
        }
    }

    #[test]
    fn segment_set_changes_are_structural_and_structural_wins() {
        assert_eq!(
            classify(
                &[("p".into(), view("ready", StyleRole::Default))],
                &[
                    ("p".into(), view("ready", StyleRole::Default)),
                    ("q".into(), view("ok", StyleRole::Default)),
                ],
            ),
            PluginDamage::Structural
        );
        let mut damage = PluginDamage::StatusbarContent;
        damage.merge(PluginDamage::Structural);
        assert_eq!(damage, PluginDamage::Structural);
    }

    #[test]
    fn label_or_order_changes_are_structural_even_when_widths_match() {
        let first = [
            ("a".into(), view("one", StyleRole::Default)),
            ("b".into(), view("two", StyleRole::Default)),
        ];
        let renamed = [
            ("renamed".into(), view("one", StyleRole::Default)),
            ("b".into(), view("two", StyleRole::Default)),
        ];
        let reordered = [
            ("b".into(), view("two", StyleRole::Default)),
            ("a".into(), view("one", StyleRole::Default)),
        ];
        assert_eq!(classify(&first, &renamed), PluginDamage::Structural);
        assert_eq!(classify(&first, &reordered), PluginDamage::Structural);
    }

    #[test]
    fn bounded_statusbar_diff_is_smaller_than_a_structural_screen_diff() {
        let mut front = Surface::new(24, 4);
        front.add_change(Change::CursorPosition {
            x: termwiz::surface::Position::Absolute(4),
            y: termwiz::surface::Position::Absolute(3),
        });
        front.add_change(Change::Text("ready".into()));
        let mut next = front.clone();
        next.add_change(Change::CursorPosition {
            x: termwiz::surface::Position::Absolute(4),
            y: termwiz::surface::Position::Absolute(3),
        });
        next.add_change(Change::Text("error".into()));

        let mut structural = next.clone();
        structural.add_change(Change::CursorPosition {
            x: Position::Absolute(0),
            y: Position::Absolute(0),
        });
        structural.add_change(Change::Text("layout changed".into()));

        let bounded = diff_bytes(&front, &next, 3, 24);
        let full = change_stats(&front.diff_screens(&structural));
        assert_eq!(bounded.0, 1, "content damage stays on one statusbar row");
        assert!(bounded.1 > 0, "changed statusbar content emits bytes");
        assert!(
            full.0 > bounded.0,
            "structural damage reaches multiple rows"
        );
        assert!(full.1 > bounded.1, "structural diff emits more bytes");
    }
}
