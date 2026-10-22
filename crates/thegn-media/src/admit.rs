//! Shared canonical bounds applied at the public backend-resolution funnel.

use std::time::Duration;

use futures::future::BoxFuture;

use crate::model::{self, LoopMode, MediaState, Playlist, PlaylistPage, QueueItem, QueuePage};
use crate::{MediaBackend, MediaCaps, MediaClient, MediaError, MediaSnapshot, MediaWatch};

/// A media backend whose provider data and opaque IDs pass canonical admission.
pub(crate) struct Admitted(pub(crate) MediaClient);

fn reject_id() -> MediaError {
    MediaError::Backend("media id rejected".into())
}

impl MediaBackend for Admitted {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            self.0
                .snapshot()
                .await
                .map(|state| state.map(MediaState::admitted))
        })
    }

    fn snapshot_with_caps(&self) -> BoxFuture<'_, Result<Option<MediaSnapshot>, MediaError>> {
        Box::pin(async move {
            self.0.snapshot_with_caps().await.map(|snapshot| {
                snapshot.map(|mut snapshot| {
                    snapshot.state = snapshot.state.admitted();
                    snapshot
                })
            })
        })
    }

    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.play_pause()
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.next()
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.previous()
    }
    fn set_shuffle(&self, on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.set_shuffle(on)
    }
    fn set_loop(&self, mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.set_loop(mode)
    }
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.volume_step(delta)
    }
    fn playlists(&self) -> BoxFuture<'_, Result<Vec<Playlist>, MediaError>> {
        Box::pin(async move {
            self.0.playlists().await.map(|items| {
                items
                    .into_iter()
                    .take(model::MAX_PLAYLISTS)
                    .filter_map(Playlist::admitted)
                    .collect()
            })
        })
    }
    /// The total may include rows dropped during admission because their IDs are invalid.
    fn playlist_page(&self) -> BoxFuture<'_, Result<PlaylistPage, MediaError>> {
        Box::pin(async move {
            let page = self.0.playlist_page().await?;
            Ok(PlaylistPage {
                total: page.total.max(page.items.len()),
                items: page
                    .items
                    .into_iter()
                    .take(model::MAX_PLAYLISTS)
                    .filter_map(Playlist::admitted)
                    .collect(),
            })
        })
    }
    fn activate_playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        if !model::is_valid_opaque_id(id) {
            return Box::pin(async { Err(reject_id()) });
        }
        self.0.activate_playlist(id)
    }
    fn seek(&self, offset: Duration, forward: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.seek(offset, forward)
    }
    fn set_position<'a>(
        &'a self,
        pos: Duration,
        track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        if track_id.is_some_and(|id| !model::is_valid_opaque_id(id)) {
            return Box::pin(async { Err(reject_id()) });
        }
        self.0.set_position(pos, track_id)
    }
    fn set_volume(&self, level: u8) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.set_volume(level)
    }
    fn queue(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, MediaError>> {
        Box::pin(async move {
            self.0.queue().await.map(|items| {
                items
                    .into_iter()
                    .take(model::MAX_QUEUE_ITEMS)
                    .filter_map(QueueItem::admitted)
                    .collect()
            })
        })
    }
    /// The total may include rows dropped during admission because their IDs are invalid.
    fn queue_page(&self) -> BoxFuture<'_, Result<QueuePage, MediaError>> {
        Box::pin(async move {
            let page = self.0.queue_page().await?;
            Ok(QueuePage {
                total: page.total.max(page.items.len()),
                items: page
                    .items
                    .into_iter()
                    .take(model::MAX_QUEUE_ITEMS)
                    .filter_map(QueueItem::admitted)
                    .collect(),
            })
        })
    }
    fn play_queue_item<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        if !model::is_valid_opaque_id(id) {
            return Box::pin(async { Err(reject_id()) });
        }
        self.0.play_queue_item(id)
    }
    fn chapter_next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.chapter_next()
    }
    fn chapter_prev(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.chapter_prev()
    }
    fn toggle_fullscreen(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        self.0.toggle_fullscreen()
    }
    fn caps(&self) -> MediaCaps {
        self.0.caps()
    }
    fn watch(&self) -> BoxFuture<'_, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>> {
        self.0.watch()
    }
    fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
        Box::pin(async move {
            let names = self.0.players().await?;
            Ok(names
                .into_iter()
                .take(model::MAX_PLAYERS)
                .filter(|name| model::is_valid_opaque_id(name))
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Fake {
        calls: std::sync::Arc<Mutex<Vec<String>>>,
        items: Mutex<Vec<QueueItem>>,
        playlists: Vec<Playlist>,
        caps: MediaCaps,
        players: Result<Vec<String>, MediaError>,
    }

    impl MediaBackend for Fake {
        fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
            Box::pin(async {
                Ok(Some(MediaState {
                    title: "t".repeat(model::MAX_FIELD_BYTES + 1),
                    track_id: Some("bad\nid".into()),
                    ..Default::default()
                }))
            })
        }
        fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            Box::pin(async { Ok(()) })
        }
        fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            Box::pin(async { Ok(()) })
        }
        fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
            Box::pin(async { Ok(()) })
        }
        fn set_shuffle(&self, _on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
            Box::pin(async { Ok(()) })
        }
        fn set_loop(&self, _mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
            Box::pin(async { Ok(()) })
        }
        fn volume_step(&self, _delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
            Box::pin(async { Ok(()) })
        }
        fn playlists(&self) -> BoxFuture<'_, Result<Vec<Playlist>, MediaError>> {
            Box::pin(async move { Ok(self.playlists.clone()) })
        }
        fn activate_playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
            self.calls.lock().unwrap().push(id.to_owned());
            Box::pin(async { Ok(()) })
        }
        fn caps(&self) -> MediaCaps {
            self.caps
        }
        fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
            Box::pin(async move { self.players.clone() })
        }
        fn queue(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, MediaError>> {
            let items = self.items.lock().unwrap().clone();
            Box::pin(async move { Ok(items) })
        }
        fn play_queue_item<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
            self.calls.lock().unwrap().push(id.to_owned());
            Box::pin(async { Ok(()) })
        }
    }

    fn fake() -> (Admitted, std::sync::Arc<Mutex<Vec<String>>>) {
        fake_with_players(Ok(Vec::new()))
    }

    fn fake_with_players(
        players: Result<Vec<String>, MediaError>,
    ) -> (Admitted, std::sync::Arc<Mutex<Vec<String>>>) {
        fake_with_items(
            players,
            vec![
                QueueItem {
                    id: "first".into(),
                    ..Default::default()
                },
                QueueItem {
                    id: String::new(),
                    ..Default::default()
                },
                QueueItem {
                    id: "last".into(),
                    ..Default::default()
                },
            ],
        )
    }

    fn fake_with_items(
        players: Result<Vec<String>, MediaError>,
        items: Vec<QueueItem>,
    ) -> (Admitted, std::sync::Arc<Mutex<Vec<String>>>) {
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let client: MediaClient = Box::new(Fake {
            calls: calls.clone(),
            items: Mutex::new(items),
            playlists: Vec::new(),
            caps: MediaCaps {
                queue: true,
                signals: true,
                ..Default::default()
            },
            players,
        });
        (Admitted(client), calls)
    }

    #[tokio::test]
    async fn ids_are_rejected_before_backend_and_valid_ids_round_trip() {
        let (admitted, fake) = fake();
        let id = "a".repeat(model::MAX_ID_BYTES + 1);
        assert_eq!(
            admitted.play_queue_item(&id).await,
            Err(MediaError::Backend("media id rejected".into()))
        );
        assert!(fake.lock().unwrap().is_empty());

        let valid = "queue/item";
        admitted.play_queue_item(valid).await.unwrap();
        assert_eq!(fake.lock().unwrap().as_slice(), [valid]);
    }

    #[tokio::test]
    async fn queue_drops_bad_ids_in_order_and_snapshot_keeps_caps() {
        let (admitted, _) = fake();
        let rows = admitted.queue().await.unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["first", "last"]
        );

        let snapshot = admitted.snapshot_with_caps().await.unwrap().unwrap();
        assert_eq!(snapshot.caps, admitted.caps());
        assert_eq!(snapshot.state.title.len(), model::MAX_FIELD_BYTES);
        assert!(snapshot.state.track_id.is_none());
    }

    #[tokio::test]
    async fn queue_page_is_capped_at_ingress_and_keeps_the_reported_total() {
        let rows = (0..model::MAX_QUEUE_ITEMS + 3)
            .map(|index| QueueItem {
                id: format!("row-{index}"),
                ..Default::default()
            })
            .collect();
        let (admitted, _) = fake_with_items(Ok(Vec::new()), rows);

        let page = admitted.queue_page().await.unwrap();
        assert_eq!(page.items.len(), model::MAX_QUEUE_ITEMS);
        assert_eq!(page.total, model::MAX_QUEUE_ITEMS + 3);
        assert_eq!(
            admitted.queue().await.unwrap().len(),
            model::MAX_QUEUE_ITEMS
        );

        let rows = (0..model::MAX_QUEUE_ITEMS + 1)
            .map(|index| QueueItem {
                id: format!("cap-row-{index}"),
                ..Default::default()
            })
            .collect();
        let (admitted, _) = fake_with_items(Ok(Vec::new()), rows);
        let page = admitted.queue_page().await.unwrap();
        assert_eq!(page.items.len(), model::MAX_QUEUE_ITEMS);
        assert_eq!(page.total, model::MAX_QUEUE_ITEMS + 1);
    }

    #[tokio::test]
    async fn playlists_and_players_are_capped_at_ingress() {
        let playlists = (0..model::MAX_PLAYLISTS + 3)
            .map(|index| Playlist {
                id: format!("pl-{index}"),
                name: format!("Playlist {index}"),
            })
            .collect();
        let players = Ok((0..model::MAX_PLAYERS + 3)
            .map(|index| format!("player-{index}"))
            .collect());
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let client: MediaClient = Box::new(Fake {
            calls: calls.clone(),
            items: Mutex::new(Vec::new()),
            playlists,
            caps: MediaCaps::default(),
            players,
        });
        let admitted = Admitted(client);

        let playlists = admitted.playlists().await.unwrap();
        assert_eq!(playlists.len(), model::MAX_PLAYLISTS);
        assert_eq!(
            playlists.first().map(|playlist| playlist.id.as_str()),
            Some("pl-0")
        );
        assert_eq!(
            playlists.last().map(|playlist| playlist.id.clone()),
            Some(format!("pl-{}", model::MAX_PLAYLISTS - 1))
        );
        assert_eq!(admitted.players().await.unwrap().len(), model::MAX_PLAYERS);
    }

    #[tokio::test]
    async fn playlist_page_is_capped_at_ingress_and_keeps_the_reported_total() {
        let playlists = (0..model::MAX_PLAYLISTS + 3)
            .map(|index| Playlist {
                id: format!("playlist-{index}"),
                name: format!("Playlist {index}"),
            })
            .collect();
        let client: MediaClient = Box::new(Fake {
            calls: std::sync::Arc::new(Mutex::new(Vec::new())),
            items: Mutex::new(Vec::new()),
            playlists,
            caps: MediaCaps::default(),
            players: Ok(Vec::new()),
        });

        let page = Admitted(client).playlist_page().await.unwrap();
        assert_eq!(page.items.len(), model::MAX_PLAYLISTS);
        assert_eq!(page.total, model::MAX_PLAYLISTS + 3);

        let client: MediaClient = Box::new(Fake {
            calls: std::sync::Arc::new(Mutex::new(Vec::new())),
            items: Mutex::new(Vec::new()),
            playlists: vec![
                Playlist {
                    id: "bad\nid".into(),
                    name: "Rejected".into(),
                },
                Playlist {
                    id: "valid-id".into(),
                    name: "Kept".into(),
                },
            ],
            caps: MediaCaps::default(),
            players: Ok(Vec::new()),
        });
        let page = Admitted(client).playlist_page().await.unwrap();
        assert_eq!(page.total, 2);
        assert!(page.total >= page.items.len());
        assert_eq!(page.items.len(), 1);
    }

    #[tokio::test]
    async fn typed_players_error_is_not_filtered_to_empty() {
        let error = MediaError::Unavailable("bus disconnected".into());
        let (admitted, _) = fake_with_players(Err(error.clone()));
        assert_eq!(admitted.players().await, Err(error));
    }
}
