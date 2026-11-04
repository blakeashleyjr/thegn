//! Aggregate decoded-allocation ceilings for an admitted media snapshot.
//! Pure: no I/O. The per-field and per-row caps live in [`crate::model`]; this
//! module adds them up and counts the bytes an admitted value actually holds.

use crate::model::{
    MAX_FIELD_BYTES, MAX_ID_BYTES, MAX_PLAYLISTS, MAX_QUEUE_ITEMS, MediaState, PlaylistPage,
    QueuePage,
};

/// Worst-case UTF-8 payload bytes of one admitted [`MediaState`]: player,
/// title, artist, album and art URL at the field cap plus a track id at the id
/// cap. Excludes struct and allocator overhead.
pub const MAX_STATE_BYTES: usize = 5 * MAX_FIELD_BYTES + MAX_ID_BYTES;
/// Worst-case UTF-8 payload bytes of one admitted queue row: id plus title plus
/// artist.
pub const MAX_QUEUE_ROW_BYTES: usize = MAX_ID_BYTES + 2 * MAX_FIELD_BYTES;
/// Worst-case UTF-8 payload bytes of one admitted queue page.
pub const MAX_QUEUE_PAGE_BYTES: usize = MAX_QUEUE_ITEMS * MAX_QUEUE_ROW_BYTES;
/// Worst-case UTF-8 payload bytes of one admitted playlist row: id plus name.
pub const MAX_PLAYLIST_ROW_BYTES: usize = MAX_ID_BYTES + MAX_FIELD_BYTES;
/// Worst-case UTF-8 payload bytes of one admitted playlist page.
pub const MAX_PLAYLIST_PAGE_BYTES: usize = MAX_PLAYLISTS * MAX_PLAYLIST_ROW_BYTES;
/// Worst-case UTF-8 payload bytes of a state, queue page and playlist page
/// held together.
pub const MAX_SNAPSHOT_BYTES: usize =
    MAX_STATE_BYTES + MAX_QUEUE_PAGE_BYTES + MAX_PLAYLIST_PAGE_BYTES;

/// UTF-8 bytes of the strings an admitted state holds (string payloads only,
/// not struct overhead).
pub fn state_bytes(state: &MediaState) -> usize {
    state.player.len()
        + state.title.len()
        + state.artist.len()
        + state.album.len()
        + state.art_url.as_deref().map_or(0, str::len)
        + state.track_id.as_deref().map_or(0, str::len)
}

/// UTF-8 bytes of the strings an admitted queue page holds (payloads only).
pub fn queue_page_bytes(page: &QueuePage) -> usize {
    page.items
        .iter()
        .map(|item| item.id.len() + item.title.len() + item.artist.len())
        .sum()
}

/// UTF-8 bytes of the strings an admitted playlist page holds (payloads only).
pub fn playlist_page_bytes(page: &PlaylistPage) -> usize {
    page.items
        .iter()
        .map(|playlist| playlist.id.len() + playlist.name.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Playlist, QueueItem};

    #[test]
    #[allow(
        clippy::disallowed_macros,
        reason = "the measured aggregate is printed for the primary to read"
    )]
    fn worst_case_admitted_snapshot_equals_the_derived_ceiling() {
        let state = MediaState {
            player: "a".repeat(MAX_FIELD_BYTES * 2),
            title: "a".repeat(MAX_FIELD_BYTES * 2),
            artist: "a".repeat(MAX_FIELD_BYTES * 2),
            album: "a".repeat(MAX_FIELD_BYTES * 2),
            art_url: Some("u".repeat(MAX_FIELD_BYTES)),
            track_id: Some("t".repeat(MAX_ID_BYTES)),
            ..Default::default()
        }
        .admitted();
        let state_bytes = state_bytes(&state);
        assert_eq!(state_bytes, MAX_STATE_BYTES);

        let queue_rows = (0..MAX_QUEUE_ITEMS + 5)
            .map(|n| QueueItem {
                id: format!("{n:0>width$}", width = MAX_ID_BYTES),
                title: "x".repeat(MAX_FIELD_BYTES * 2),
                artist: "x".repeat(MAX_FIELD_BYTES * 2),
                ..Default::default()
            })
            .filter_map(QueueItem::admitted)
            .collect();
        let queue_page = QueuePage::from_rows(queue_rows);
        let queue_page_bytes = queue_page_bytes(&queue_page);
        assert_eq!(queue_page_bytes, MAX_QUEUE_PAGE_BYTES);
        assert_eq!(queue_page.items.len(), MAX_QUEUE_ITEMS);

        let playlist_rows = (0..MAX_PLAYLISTS + 5)
            .map(|n| Playlist {
                id: format!("{n:0>width$}", width = MAX_ID_BYTES),
                name: "x".repeat(MAX_FIELD_BYTES * 2),
            })
            .filter_map(Playlist::admitted)
            .collect();
        let playlist_page = PlaylistPage::from_rows(playlist_rows);
        let playlist_page_bytes = playlist_page_bytes(&playlist_page);
        assert_eq!(playlist_page_bytes, MAX_PLAYLIST_PAGE_BYTES);

        let total = state_bytes + queue_page_bytes + playlist_page_bytes;
        eprintln!(
            "measured media snapshot: state={} queue_page={} playlist_page={} total={}",
            state_bytes, queue_page_bytes, playlist_page_bytes, total
        );
        assert_eq!(total, MAX_SNAPSHOT_BYTES);
    }

    #[test]
    fn multibyte_fields_stay_within_the_ceiling() {
        let field = "\u{20ac}".repeat(MAX_FIELD_BYTES);
        let state = MediaState {
            player: field.clone(),
            title: field.clone(),
            artist: field.clone(),
            album: field,
            art_url: Some("\u{e9}".repeat(MAX_FIELD_BYTES / 2)),
            ..Default::default()
        }
        .admitted();

        assert!(state_bytes(&state) <= MAX_STATE_BYTES);
        assert!(state.player.len() <= MAX_FIELD_BYTES);
        assert!(state.title.len() <= MAX_FIELD_BYTES);
        assert!(state.artist.len() <= MAX_FIELD_BYTES);
        assert!(state.album.len() <= MAX_FIELD_BYTES);
        assert_eq!(state.title.len(), MAX_FIELD_BYTES - 1);
        assert!(state.title.is_char_boundary(state.title.len()));

        let multibyte_field = "\u{e9}".repeat(MAX_FIELD_BYTES);
        let queue_page = QueuePage::from_rows(vec![
            QueueItem {
                id: "id".to_owned(),
                title: multibyte_field.clone(),
                artist: multibyte_field.clone(),
                ..Default::default()
            }
            .admitted()
            .expect("non-empty queue item id is admitted"),
        ]);
        let playlist_page = PlaylistPage::from_rows(vec![
            Playlist {
                id: "id".to_owned(),
                name: multibyte_field,
            }
            .admitted()
            .expect("non-empty playlist id is admitted"),
        ]);
        assert!(queue_page_bytes(&queue_page) <= MAX_QUEUE_PAGE_BYTES);
        assert!(playlist_page_bytes(&playlist_page) <= MAX_PLAYLIST_PAGE_BYTES);
        assert!(
            state_bytes(&state)
                + queue_page_bytes(&queue_page)
                + playlist_page_bytes(&playlist_page)
                <= MAX_SNAPSHOT_BYTES,
            "multi-byte admitted fields across state and pages must stay within aggregate ceiling"
        );
    }

    #[test]
    fn empty_snapshot_costs_zero_bytes() {
        assert_eq!(state_bytes(&MediaState::default()), 0);
        assert_eq!(queue_page_bytes(&QueuePage::default()), 0);
        assert_eq!(playlist_page_bytes(&PlaylistPage::default()), 0);
    }

    #[test]
    fn aggregate_ceiling_is_the_sum_of_its_parts() {
        assert_eq!(
            MAX_SNAPSHOT_BYTES,
            MAX_STATE_BYTES + MAX_QUEUE_PAGE_BYTES + MAX_PLAYLIST_PAGE_BYTES
        );
        assert_eq!(MAX_STATE_BYTES, 21_504);
        assert_eq!(MAX_QUEUE_ROW_BYTES, 9_216);
        assert_eq!(MAX_QUEUE_PAGE_BYTES, 4_608_000);
        assert_eq!(MAX_PLAYLIST_ROW_BYTES, 5_120);
        assert_eq!(MAX_PLAYLIST_PAGE_BYTES, 512_000);
        assert_eq!(MAX_SNAPSHOT_BYTES, 5_141_504);
    }
}
