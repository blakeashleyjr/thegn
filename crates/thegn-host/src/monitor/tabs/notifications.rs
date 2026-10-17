//! Notifications tab renderer and tab-specific monitor actions.

use super::super::build::*;
use crate::sections::{Cell, Section, TableSection};
use crate::seg::Tok;
use thegn_core::theme::Hue;

/// Build the conditional Notifications tab from the worker's counter-only
/// snapshot. Endpoints and payloads never cross into the render model.
pub(in crate::monitor) fn notifications(cx: &Ctx<'_>) -> TabBuild {
    let rows = cx.model.notification_delivery.rows();
    if rows.is_empty() {
        return plain(vec![heading(
            "delivery",
            Some("no configured sinks".into()),
        )]);
    }
    let table_rows = rows
        .into_iter()
        .map(|row| {
            let outcome_tone = if row.dead_letters > 0 {
                Tok::Hue(Hue::Red)
            } else {
                Tok::Slot(S::Text)
            };
            vec![
                Cell::Text(trunc(&row.name, 18), Tok::Slot(S::Text)),
                Cell::Text(trunc(&row.kind, 10), Tok::Slot(S::Ghost)),
                Cell::Text(row.queue_drops.to_string(), Tok::Slot(S::Dim)),
                Cell::Text(row.rate_limit_drops.to_string(), Tok::Slot(S::Dim)),
                Cell::Text(row.retries.to_string(), Tok::Slot(S::Dim)),
                Cell::Text(row.sent.to_string(), Tok::Slot(S::Text)),
                Cell::Text(row.dead_letters.to_string(), outcome_tone),
            ]
        })
        .collect();
    plain(vec![
        heading("delivery", Some("bounded, best-effort".into())),
        Section::Table(TableSection {
            header: vec![
                "sink".into(),
                "kind".into(),
                "queue drop".into(),
                "rate drop".into(),
                "retry".into(),
                "sent".into(),
                "dead-letter".into(),
            ],
            rows: table_rows,
            sel: None,
        }),
    ])
}
