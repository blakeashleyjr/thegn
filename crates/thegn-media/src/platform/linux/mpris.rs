//! Native MPRIS backend over D-Bus (`zbus`). Implements [`MediaBackend`] against
//! the `org.mpris.MediaPlayer2{,.Player,.Playlists}` interfaces every compliant
//! player exposes, plus a push [`MprisWatch`] built from D-Bus signals so the
//! host updates on change without polling.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use zbus::message::Type as MsgType;
use zbus::names::InterfaceName;
use zbus::zvariant::{Array, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use crate::model::{LoopMode, MediaKind, MediaState, PlaybackState, Playlist, QueueItem};
use crate::{MediaBackend, MediaCaps, MediaError, MediaSnapshot, MediaWatch};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const PLAYLISTS_IFACE: &str = "org.mpris.MediaPlayer2.Playlists";
const TRACKLIST_IFACE: &str = "org.mpris.MediaPlayer2.TrackList";

type PlayerPropertyRead = (String, Result<HashMap<String, OwnedValue>, MediaError>);

/// A connected MPRIS controller. Holds one session-bus connection; the active
/// player is re-resolved per call so launching/quitting a player is picked up.
pub struct MprisZbus {
    conn: Connection,
    /// Preferred player tails (e.g. `"spotify"`); first match wins.
    priority: Vec<String>,
    /// The bus name we last resolved as active. When two players are on the bus
    /// at once (e.g. mpv + mpd), we *stick* to this one while it stays
    /// playing/paused so the badge doesn't flicker between them on every D-Bus
    /// signal. Interior-mutable because `snapshot`/controls take `&self`.
    last: std::sync::Mutex<Option<String>>,
}

impl MprisZbus {
    /// Open the D-Bus **session** bus. `Err` ⇒ the caller falls back to
    /// `playerctl` (or shows nothing).
    pub async fn connect(priority: Vec<String>) -> Result<Self, MediaError> {
        let conn = Connection::session()
            .await
            .map_err(|e| MediaError::Unavailable(format!("session bus: {e}")))?;
        Ok(Self {
            conn,
            priority,
            last: std::sync::Mutex::new(None),
        })
    }

    /// All `org.mpris.MediaPlayer2.*` bus names currently on the bus.
    async fn player_bus_names(&self) -> Result<Vec<String>, MediaError> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn)
            .await
            .map_err(|e| MediaError::Unavailable(e.to_string()))?;
        let names = dbus
            .list_names()
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        let mut buses: Vec<String> = names
            .into_iter()
            .map(|n| n.as_str().to_string())
            .filter(|n| n.starts_with(MPRIS_PREFIX))
            .collect();
        // `list_names()` order is arbitrary; sort for a deterministic pick so
        // selection (and the picker list) doesn't drift between calls.
        buses.sort();
        Ok(buses)
    }

    /// The controllable players' short tails (for the picker UI).
    pub async fn list_players(&self) -> Result<Vec<String>, MediaError> {
        Ok(self
            .player_bus_names()
            .await?
            .iter()
            .map(|n| tail(n).to_string())
            .collect())
    }

    /// A `Properties` proxy for one player.
    async fn props_proxy(&self, bus: &str) -> Result<zbus::fdo::PropertiesProxy<'_>, MediaError> {
        zbus::fdo::PropertiesProxy::builder(&self.conn)
            .destination(bus.to_string())
            .map_err(|e| MediaError::Backend(e.to_string()))?
            .path(MPRIS_PATH)
            .map_err(|e| MediaError::Backend(e.to_string()))?
            .build()
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    /// All `Player` properties for one player, in one round-trip.
    async fn player_props(&self, bus: &str) -> Result<HashMap<String, OwnedValue>, MediaError> {
        let proxy = self.props_proxy(bus).await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE)
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        proxy
            .get_all(iface)
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    /// Read player properties for a mutation, retaining owner-loss errors as stale.
    async fn player_props_for_mutation(
        &self,
        bus: &str,
    ) -> Result<HashMap<String, OwnedValue>, MediaError> {
        let proxy = self.props_proxy(bus).await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE)
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        proxy.get_all(iface).await.map_err(map_mutation_fdo_error)
    }

    /// Resolve and read the selected player once for either snapshot API.
    async fn active_snapshot_data(
        &self,
    ) -> Result<Option<(String, HashMap<String, OwnedValue>)>, MediaError> {
        let Some(bus) = self.active_player().await? else {
            tracing::debug!(target: "thegn::media", "MPRIS: no active player on the bus");
            return Ok(None);
        };
        let props = self.player_props(&bus).await?;
        Ok(Some((bus, props)))
    }

    async fn root_props(&self, bus: &str) -> Result<HashMap<String, OwnedValue>, MediaError> {
        let proxy = self.props_proxy(bus).await?;
        let iface =
            InterfaceName::try_from(ROOT_IFACE).map_err(|e| MediaError::Backend(e.to_string()))?;
        proxy
            .get_all(iface)
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    async fn has_playlists(&self, bus: &str) -> Result<bool, MediaError> {
        let proxy = self.props_proxy(bus).await?;
        let iface = InterfaceName::try_from(PLAYLISTS_IFACE)
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        proxy
            .get(iface, "PlaylistCount")
            .await
            .map(|_| true)
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    /// Resolve which player to control. Reads each present player's state once,
    /// then delegates the choice to the pure [`choose_player`] (priority →
    /// sticky-if-active → first-playing → sticky-if-present → first-present) and
    /// records the winner so the next call can stick to it.
    async fn active_player(&self) -> Result<Option<String>, MediaError> {
        let names = self.player_bus_names().await?;
        if names.is_empty() {
            // best-effort: clear the sticky pick; the mutex is only poisoned if a
            // holder panicked, which can't happen (no panic path under the lock).
            if let Ok(mut last) = self.last.lock() {
                *last = None;
            }
            return Ok(None);
        }
        let mut reads = Vec::with_capacity(names.len());
        for n in names {
            reads.push((n.clone(), self.player_props(&n).await));
        }
        let candidates = readable_candidates(reads)?;
        let sticky = self.last.lock().ok().and_then(|g| g.clone());
        let chosen = choose_player(&candidates, &self.priority, sticky.as_deref());
        if let Ok(mut last) = self.last.lock() {
            *last = chosen.clone();
        }
        Ok(chosen)
    }

    /// Resolve a well-known MPRIS name to the process that currently owns it.
    async fn owner_of(&self, bus: &str) -> Result<String, MediaError> {
        let name =
            zbus::names::BusName::try_from(bus).map_err(|e| MediaError::Backend(e.to_string()))?;
        let dbus = zbus::fdo::DBusProxy::new(&self.conn)
            .await
            .map_err(|e| MediaError::Unavailable(e.to_string()))?;
        dbus.get_name_owner(name)
            .await
            .map(|owner| owner.as_str().to_string())
            .map_err(|error| {
                if is_owner_gone_fdo_error(&error) {
                    MediaError::Stale(format!("{bus} has no current owner"))
                } else {
                    MediaError::Unavailable(error.to_string())
                }
            })
    }

    /// Select a well-known player and pin this call to its current unique owner.
    async fn selected_owner(&self) -> Result<(String, String), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        let owner = self.owner_of(&bus).await?;
        Ok((bus, owner))
    }

    /// Invoke a no-argument `Player` method (PlayPause / Next / Previous).
    async fn player_call(&self, bus: &str, member: &str) -> Result<(), MediaError> {
        self.conn
            .call_method(Some(bus), MPRIS_PATH, Some(PLAYER_IFACE), member, &())
            .await
            .map(|_| ())
            .map_err(map_mutation_method_error)
    }

    /// Set a `Player` property.
    async fn player_set(&self, bus: &str, prop: &str, value: Value<'_>) -> Result<(), MediaError> {
        let proxy = self.props_proxy(bus).await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE)
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        proxy
            .set(iface, prop, value)
            .await
            .map_err(map_mutation_fdo_error)
    }

    /// Build a push watcher over MPRIS-relevant D-Bus signals.
    pub async fn watch(&self) -> Result<MprisWatch, MediaError> {
        // Scope the properties rule to the MPRIS object path — the spec fixes
        // every player at `/org/mpris/MediaPlayer2`. Without it this matches
        // EVERY `PropertiesChanged` on the session bus (e.g. `systemd --user`
        // broadcasting podman unit churn caused by our own container polling),
        // waking the watcher ~20×/5s for a full aggregate re-snapshot each time.
        let props_rule = MatchRule::builder()
            .msg_type(MsgType::Signal)
            .path(MPRIS_PATH)
            .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
            .and_then(|b| b.member("PropertiesChanged"))
            .map_err(|e| MediaError::Backend(e.to_string()))?
            .build();
        let names_rule = MatchRule::builder()
            .msg_type(MsgType::Signal)
            .interface("org.freedesktop.DBus")
            .and_then(|b| b.member("NameOwnerChanged"))
            .and_then(|b| b.arg0ns("org.mpris.MediaPlayer2"))
            .map_err(|e| MediaError::Backend(e.to_string()))?
            .build();
        let props = MessageStream::for_match_rule(props_rule, &self.conn, Some(64))
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        let names = MessageStream::for_match_rule(names_rule, &self.conn, Some(16))
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        Ok(MprisWatch { props, names })
    }
}

impl MediaBackend for MprisZbus {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            let Some((bus, props)) = self.active_snapshot_data().await? else {
                return Ok(None);
            };
            let state = parse_state(tail(&bus), &props);
            tracing::debug!(
                target: "thegn::media",
                bus = %bus, state = ?state.state, title = %state.title, artist = %state.artist,
                "MPRIS snapshot",
            );
            Ok(Some(state))
        })
    }

    fn snapshot_with_caps(&self) -> BoxFuture<'_, Result<Option<MediaSnapshot>, MediaError>> {
        Box::pin(async move {
            let Some((bus, player)) = self.active_snapshot_data().await? else {
                return Ok(None);
            };
            let (root, playlists) = futures::join!(self.root_props(&bus), self.has_playlists(&bus));
            let root = match root {
                Ok(props) => Some(props),
                Err(error) => {
                    tracing::debug!(target: "thegn::media", bus = %bus, %error, "MPRIS root capabilities unavailable");
                    None
                }
            };
            let playlists = match playlists {
                Ok(available) => available,
                Err(error) => {
                    tracing::debug!(target: "thegn::media", bus = %bus, %error, "MPRIS playlist capability unavailable");
                    false
                }
            };
            let state = parse_state(tail(&bus), &player);
            let caps = caps_from_props(&player, root.as_ref(), playlists);
            tracing::debug!(
                target: "thegn::media",
                bus = %bus, state = ?state.state, title = %state.title, artist = %state.artist,
                "MPRIS snapshot with capabilities",
            );
            Ok(Some(MediaSnapshot { state, caps }))
        })
    }

    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            self.player_call(&owner, "PlayPause").await
        })
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            self.player_call(&owner, "Next").await
        })
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            self.player_call(&owner, "Previous").await
        })
    }
    fn set_shuffle(&self, on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            self.player_set(&owner, "Shuffle", Value::Bool(on)).await
        })
    }
    fn set_loop(&self, mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            self.player_set(&owner, "LoopStatus", Value::from(mode.as_mpris()))
                .await
        })
    }
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (bus, owner) = self.selected_owner().await?;
            let props = self.player_props_for_mutation(&owner).await?;
            // A missing Volume must not be replaced by a guess and written back.
            let next = stepped_volume(f64_of(&props, "Volume"), delta)?;
            write_if_same_owner(&owner, self.owner_of(&bus).await, || async {
                self.player_set(&owner, "Volume", Value::F64(next)).await
            })
            .await
        })
    }

    fn playlists(&self) -> BoxFuture<'_, Result<Vec<Playlist>, MediaError>> {
        Box::pin(async move {
            let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
            // GetPlaylists(index, max, order, reverse) -> a(oss)
            let reply = self
                .conn
                .call_method(
                    Some(bus.as_str()),
                    MPRIS_PATH,
                    Some(PLAYLISTS_IFACE),
                    "GetPlaylists",
                    &(0u32, 100u32, "Alphabetical", false),
                )
                .await
                .map_err(|e| MediaError::Backend(e.to_string()))?;
            let lists: Vec<(OwnedObjectPath, String, String)> = reply
                .body()
                .deserialize()
                .map_err(|e| MediaError::Backend(e.to_string()))?;
            Ok(lists
                .into_iter()
                .map(|(path, name, _icon)| Playlist {
                    id: path.as_str().to_string(),
                    name,
                })
                .collect())
        })
    }

    fn activate_playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            let path =
                OwnedObjectPath::try_from(id).map_err(|e| MediaError::Backend(e.to_string()))?;
            self.conn
                .call_method(
                    Some(owner.as_str()),
                    MPRIS_PATH,
                    Some(PLAYLISTS_IFACE),
                    "ActivatePlaylist",
                    &(path,),
                )
                .await
                .map(|_| ())
                .map_err(map_mutation_method_error)
        })
    }

    fn seek(&self, offset: Duration, forward: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            let micros = offset.as_micros().min(i64::MAX as u128) as i64;
            let signed = if forward { micros } else { -micros };
            // Player.Seek(Offset: x)
            self.conn
                .call_method(
                    Some(owner.as_str()),
                    MPRIS_PATH,
                    Some(PLAYER_IFACE),
                    "Seek",
                    &(signed,),
                )
                .await
                .map(|_| ())
                .map_err(map_mutation_method_error)
        })
    }

    fn set_position<'a>(
        &'a self,
        pos: Duration,
        track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            // Resolve the track id: prefer the caller's, else read it fresh.
            let tid = match track_id {
                Some(t) => t.to_string(),
                None => {
                    let props = self.player_props_for_mutation(&owner).await?;
                    trackid_of(&props)
                        .ok_or_else(|| MediaError::Backend("no trackid for SetPosition".into()))?
                }
            };
            let path = OwnedObjectPath::try_from(tid.as_str())
                .map_err(|e| MediaError::Backend(e.to_string()))?;
            let micros = pos.as_micros().min(i64::MAX as u128) as i64;
            // Player.SetPosition(TrackId: o, Position: x)
            self.conn
                .call_method(
                    Some(owner.as_str()),
                    MPRIS_PATH,
                    Some(PLAYER_IFACE),
                    "SetPosition",
                    &(path, micros),
                )
                .await
                .map(|_| ())
                .map_err(map_mutation_method_error)
        })
    }

    fn set_volume(&self, level: u8) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            let v = (level.min(100) as f64) / 100.0;
            self.player_set(&owner, "Volume", Value::F64(v)).await
        })
    }

    fn queue(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, MediaError>> {
        Box::pin(async move {
            let Some(bus) = self.active_player().await? else {
                return Ok(Vec::new());
            };
            // TrackList.Tracks is `ao`; read it via Properties.Get. Players without
            // the TrackList interface error here → treat as an empty queue.
            let proxy = self.props_proxy(&bus).await?;
            let iface = InterfaceName::try_from(TRACKLIST_IFACE)
                .map_err(|e| MediaError::Backend(e.to_string()))?;
            let Ok(tracks_val) = proxy.get(iface, "Tracks").await else {
                return Ok(Vec::new());
            };
            let paths: Vec<OwnedObjectPath> = match Vec::try_from(tracks_val) {
                Ok(p) => p,
                Err(_) => return Ok(Vec::new()),
            };
            if paths.is_empty() {
                return Ok(Vec::new());
            }
            // GetTracksMetadata(ao) -> aa{sv}
            let reply = match self
                .conn
                .call_method(
                    Some(bus.as_str()),
                    MPRIS_PATH,
                    Some(TRACKLIST_IFACE),
                    "GetTracksMetadata",
                    &(paths.clone(),),
                )
                .await
            {
                Ok(r) => r,
                Err(_) => return Ok(Vec::new()),
            };
            let metas: Vec<HashMap<String, OwnedValue>> = match reply.body().deserialize() {
                Ok(m) => m,
                Err(_) => return Ok(Vec::new()),
            };
            // The current track, so we can mark it in the list.
            let current = self
                .player_props(&bus)
                .await
                .ok()
                .and_then(|p| trackid_of(&p));
            Ok(metas
                .into_iter()
                .map(|meta| {
                    let id = str_of(&meta, "mpris:trackid")
                        .or_else(|| objpath_of(&meta, "mpris:trackid"))
                        .unwrap_or_default();
                    let is_current = current.as_deref() == Some(id.as_str());
                    QueueItem {
                        id,
                        title: str_of(&meta, "xesam:title").unwrap_or_default(),
                        artist: artists_of(&meta),
                        duration: meta
                            .get("mpris:length")
                            .and_then(|v| micros_of(v))
                            .filter(|n| *n > 0)
                            .map(|us| Duration::from_micros(us as u64)),
                        is_current,
                    }
                })
                .collect())
        })
    }

    fn play_queue_item<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            let (_, owner) = self.selected_owner().await?;
            let path =
                OwnedObjectPath::try_from(id).map_err(|e| MediaError::Backend(e.to_string()))?;
            let proxy = self.props_proxy(&owner).await?;
            let iface = InterfaceName::try_from(TRACKLIST_IFACE)
                .map_err(|e| MediaError::Backend(e.to_string()))?;
            let tracks_value = proxy
                .get(iface, "Tracks")
                .await
                .map_err(map_mutation_fdo_error)?;
            let tracks: Vec<OwnedObjectPath> =
                Vec::try_from(tracks_value).map_err(|e| MediaError::Backend(e.to_string()))?;
            if !queue_item_is_in_tracklist(&tracks, &path) {
                return Err(MediaError::Stale(
                    "queue item is not in the current player's track list".into(),
                ));
            }
            // TrackList.GoTo(TrackId: o)
            self.conn
                .call_method(
                    Some(owner.as_str()),
                    MPRIS_PATH,
                    Some(TRACKLIST_IFACE),
                    "GoTo",
                    &(path,),
                )
                .await
                .map(|_| ())
                .map_err(map_mutation_method_error)
        })
    }

    fn toggle_fullscreen(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (bus, owner) = self.selected_owner().await?;
            // Root MediaPlayer2.Fullscreen (writable when CanSetFullscreen); read the
            // current value and set its inverse so the op is a self-contained toggle.
            let proxy = self.props_proxy(&owner).await?;
            let iface = InterfaceName::try_from(ROOT_IFACE)
                .map_err(|e| MediaError::Backend(e.to_string()))?;
            // A failed or unparseable read must not be turned into `false` and
            // written back as `true`.
            let read = proxy.get(iface.clone(), "Fullscreen").await.map_err(|e| {
                if is_owner_gone_fdo_error(&e) {
                    return map_mutation_fdo_error(e);
                }
                let message = format!("read Fullscreen: {e}");
                if is_missing_property_error(&e) {
                    MediaError::Unsupported(message)
                } else {
                    MediaError::Backend(message)
                }
            })?;
            let cur = Result::ok(bool::try_from(read));
            let next = toggled_fullscreen(cur)?;
            write_fullscreen_if_same_owner(
                &owner,
                self.owner_of(&bus).await,
                iface,
                next,
                |iface, value| async {
                    proxy
                        .set(iface, "Fullscreen", Value::Bool(value))
                        .await
                        .map_err(map_mutation_fdo_error)
                },
            )
            .await
        })
    }

    /// Native MPRIS lists players straight off the bus.
    fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
        Box::pin(async move { self.list_players().await })
    }

    /// The D-Bus signal watcher — the native push path (no polling).
    fn watch(&self) -> BoxFuture<'_, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>> {
        Box::pin(async move {
            MprisZbus::watch(self)
                .await
                .map(|w| Some(Box::new(w) as Box<dyn MediaWatch + Send>))
        })
    }

    fn caps(&self) -> MediaCaps {
        // This is the pre-snapshot upper bound; snapshot_with_caps reports the
        // selected player's concrete capabilities.
        MediaCaps {
            shuffle: true,
            loop_mode: true,
            volume: true,
            playlists: true,
            signals: true,
            seek: true,
            art: true,
            queue: true,
            abs_volume: true,
            chapters: false,
            fullscreen: true,
        }
    }
}

/// A live D-Bus signal watcher: resolves whenever a player's playback properties
/// change or a player appears/disappears. The host re-snapshots on each tick and
/// only marks the chrome dirty when the [`MediaState`] actually changed — so no
/// polling timer is needed (the ~0%-idle contract holds).
pub struct MprisWatch {
    props: MessageStream,
    names: MessageStream,
}

impl MediaWatch for MprisWatch {
    fn changed(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            tokio::select! {
                m = self.props.next() => m.is_some(),
                m = self.names.next() => m.is_some(),
            }
        })
    }
}

// === pure decoders =========================================================

/// The tail after the `org.mpris.MediaPlayer2.` prefix (the player's short name).
fn tail(bus: &str) -> &str {
    bus.strip_prefix(MPRIS_PREFIX).unwrap_or(bus)
}

/// Next volume for a relative step. A missing or non-finite read is an error:
/// never synthesize a base value and write it.
fn stepped_volume(current: Option<f64>, delta: f64) -> Result<f64, MediaError> {
    let cur = current.filter(|value| value.is_finite()).ok_or_else(|| {
        MediaError::Unsupported("player did not report a finite Volume".to_string())
    })?;
    Ok((cur + delta).clamp(0.0, 1.0))
}

/// Inverse of the current fullscreen state; an unreadable state is an error
/// rather than an assumed `false`.
fn toggled_fullscreen(current: Option<bool>) -> Result<bool, MediaError> {
    current
        .map(|c| !c)
        .ok_or_else(|| MediaError::Backend("Fullscreen state unreadable".to_string()))
}

fn queue_item_is_in_tracklist(tracks: &[OwnedObjectPath], path: &OwnedObjectPath) -> bool {
    tracks.iter().any(|track| track == path)
}

async fn write_fullscreen_if_same_owner<W, Fut>(
    observed: &str,
    current: Result<String, MediaError>,
    iface: InterfaceName<'static>,
    value: bool,
    write: W,
) -> Result<(), MediaError>
where
    W: FnOnce(InterfaceName<'static>, bool) -> Fut,
    Fut: Future<Output = Result<(), MediaError>>,
{
    write_if_same_owner(observed, current, || write(iface, value)).await
}

async fn write_if_same_owner<W, Fut>(
    observed: &str,
    current: Result<String, MediaError>,
    write: W,
) -> Result<(), MediaError>
where
    W: FnOnce() -> Fut,
    Fut: Future<Output = Result<(), MediaError>>,
{
    let current = current?;
    if current != observed {
        return Err(MediaError::Stale(
            "changed owner between read and write; nothing was written".into(),
        ));
    }
    write().await
}

fn is_owner_gone_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner"
    )
}

fn map_mutation_method_error(error: zbus::Error) -> MediaError {
    if is_owner_gone_method_error(&error) {
        MediaError::Stale(format!("selected player owner disappeared: {error}"))
    } else {
        MediaError::Backend(error.to_string())
    }
}

fn is_owner_gone_method_error(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => is_owner_gone_name(name.as_str()),
        _ => false,
    }
}

fn map_mutation_fdo_error(error: zbus::fdo::Error) -> MediaError {
    if is_owner_gone_fdo_error(&error) {
        MediaError::Stale(format!("selected player owner disappeared: {error}"))
    } else {
        MediaError::Backend(error.to_string())
    }
}

fn is_owner_gone_fdo_error(error: &zbus::fdo::Error) -> bool {
    match error {
        zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_) => true,
        zbus::fdo::Error::ZBus(inner) => is_owner_gone_method_error(inner),
        _ => false,
    }
}

fn is_missing_property_error(error: &zbus::fdo::Error) -> bool {
    match error {
        zbus::fdo::Error::UnknownProperty(_) | zbus::fdo::Error::InvalidArgs(_) => true,
        zbus::fdo::Error::ZBus(zbus::Error::MethodError(name, _, _)) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.UnknownProperty" | "org.freedesktop.DBus.Error.InvalidArgs"
        ),
        _ => false,
    }
}

/// Pick which player to show from the present `candidates` (each a sorted
/// `(bus, state)` pair). Precedence:
///
/// 1. **priority** — the first candidate whose tail equals or contains a
///    configured `priority` entry (an explicit user pin always wins);
/// 2. **sticky** — the previously-chosen `sticky` player while it stays present
///    *and* active (playing/paused), so the badge doesn't flip between two
///    simultaneously-playing players on every D-Bus signal;
/// 3. **first playing** in sorted order;
/// 4. **sticky** if still present at all (even if momentarily stopped) — rides
///    out a brief inter-track gap without switching players;
/// 5. the first present candidate (deterministic since `candidates` is sorted).
///
/// Returns `None` only when `candidates` is empty.
fn choose_player(
    candidates: &[(String, PlaybackState)],
    priority: &[String],
    sticky: Option<&str>,
) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }
    // 1. Explicit priority pin.
    for p in priority {
        if let Some((bus, _)) = candidates
            .iter()
            .find(|(bus, _)| tail(bus) == p || bus.contains(p.as_str()))
        {
            return Some(bus.clone());
        }
    }
    // 2. Keep the sticky player while it's still active.
    if let Some(last) = sticky
        && candidates
            .iter()
            .any(|(bus, st)| bus == last && st.is_active())
    {
        return Some(last.to_string());
    }
    // 3. Otherwise prefer whatever is actually playing.
    if let Some((bus, _)) = candidates
        .iter()
        .find(|(_, st)| matches!(st, PlaybackState::Playing))
    {
        return Some(bus.clone());
    }
    // 4. Nothing playing — keep the sticky player if it's still around.
    if let Some(last) = sticky
        && candidates.iter().any(|(bus, _)| bus == last)
    {
        return Some(last.to_string());
    }
    // 5. First present (candidates are sorted).
    candidates.first().map(|(bus, _)| bus.clone())
}

/// Peel any number of nested variant (`v`) layers down to the contained value.
fn peel<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
    match v {
        Value::Value(inner) => peel(inner),
        other => other,
    }
}

fn str_of(map: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match peel(map.get(key)?) {
        Value::Str(s) => Some(s.as_str().to_string()),
        _ => None,
    }
}

/// Read an object-path value (e.g. `mpris:trackid`) as its string form.
fn objpath_of(map: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match peel(map.get(key)?) {
        Value::ObjectPath(p) => Some(p.as_str().to_string()),
        _ => None,
    }
}

/// The current track's id (`mpris:trackid`) from a `Player` property map, whether
/// the player sends it as an object path or a bare string. Reads through the
/// nested `Metadata` dict.
fn trackid_of(props: &HashMap<String, OwnedValue>) -> Option<String> {
    let meta: HashMap<String, OwnedValue> = props
        .get("Metadata")
        .and_then(|v| peel(v).try_to_owned().ok())
        .and_then(|owned| HashMap::try_from(owned).ok())?;
    objpath_of(&meta, "mpris:trackid").or_else(|| str_of(&meta, "mpris:trackid"))
}

fn bool_of(map: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    match peel(map.get(key)?) {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn f64_of(map: &HashMap<String, OwnedValue>, key: &str) -> Option<f64> {
    match peel(map.get(key)?) {
        Value::F64(x) => Some(*x),
        _ => None,
    }
}

/// Read an integer-ish value as `i64`, tolerating the various widths a player
/// might use for `mpris:length` / `Position` (microseconds).
fn micros_of(v: &Value<'_>) -> Option<i64> {
    match peel(v) {
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        Value::I32(n) => Some(*n as i64),
        Value::U32(n) => Some(*n as i64),
        Value::I16(n) => Some(*n as i64),
        Value::U16(n) => Some(*n as i64),
        _ => None,
    }
}

/// `xesam:artist` is normally `as` (a list); some players send a bare string.
fn artists_of(meta: &HashMap<String, OwnedValue>) -> String {
    let Some(v) = meta.get("xesam:artist") else {
        return String::new();
    };
    match peel(v) {
        Value::Array(arr) => collect_strs(arr).join(", "),
        Value::Str(s) => s.as_str().to_string(),
        _ => String::new(),
    }
}

fn collect_strs(arr: &Array<'_>) -> Vec<String> {
    arr.iter()
        .filter_map(|e| match peel(e) {
            Value::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        })
        .collect()
}

fn playback_state(props: &HashMap<String, OwnedValue>) -> PlaybackState {
    str_of(props, "PlaybackStatus")
        .map(|s| PlaybackState::from_mpris(&s))
        .unwrap_or(PlaybackState::Stopped)
}

/// Keep only players whose property read succeeded. If every present player
/// failed its read, surface an error so the caller can fall back to playerctl.
fn readable_candidates(
    reads: Vec<PlayerPropertyRead>,
) -> Result<Vec<(String, PlaybackState)>, MediaError> {
    let had_players = !reads.is_empty();
    let mut candidates = Vec::with_capacity(reads.len());
    for (bus, read) in reads {
        match read {
            Ok(props) => candidates.push((bus, playback_state(&props))),
            Err(error) => tracing::debug!(
                target: "thegn::media",
                bus = %bus,
                %error,
                "MPRIS player properties unreadable; skipping candidate",
            ),
        }
    }
    if had_players && candidates.is_empty() {
        return Err(MediaError::Backend(
            "no readable MPRIS player properties".into(),
        ));
    }
    Ok(candidates)
}

/// Fold a `Player` property map into a normalized [`MediaState`].
fn parse_state(player: &str, props: &HashMap<String, OwnedValue>) -> MediaState {
    // `Metadata` arrives from `Properties.GetAll` inside a variant; peel any
    // variant layers (like every other decoder here) before the dict
    // conversion, else a variant-wrapped `a{sv}` silently fails `try_from` and
    // we lose title/artist. `peel` no-ops when the value is already a bare dict.
    let meta: HashMap<String, OwnedValue> = props
        .get("Metadata")
        .and_then(|v| peel(v).try_to_owned().ok())
        .and_then(|owned| HashMap::try_from(owned).ok())
        .unwrap_or_default();

    let length = meta
        .get("mpris:length")
        .and_then(|v| micros_of(v))
        .filter(|n| *n > 0)
        .map(|us| Duration::from_micros(us as u64));
    let position = props
        .get("Position")
        .and_then(|v| micros_of(v))
        .filter(|n| *n >= 0)
        .map(|us| Duration::from_micros(us as u64));

    let url = str_of(&meta, "xesam:url");
    let mime = str_of(&meta, "mpris:mime"); // rare, but honored when present
    MediaState {
        player: player.to_string(),
        title: str_of(&meta, "xesam:title").unwrap_or_default(),
        artist: artists_of(&meta),
        album: str_of(&meta, "xesam:album").unwrap_or_default(),
        state: playback_state(props),
        position,
        length,
        shuffle: bool_of(props, "Shuffle"),
        loop_mode: str_of(props, "LoopStatus").and_then(|s| LoopMode::parse_mpris(&s)),
        volume: f64_of(props, "Volume").map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u8),
        can_go_next: bool_of(props, "CanGoNext").unwrap_or(false),
        can_go_previous: bool_of(props, "CanGoPrevious").unwrap_or(false),
        art_url: str_of(&meta, "mpris:artUrl").filter(|s| !s.is_empty()),
        kind: MediaKind::from_hints(player, mime.as_deref(), url.as_deref()),
        can_seek: bool_of(props, "CanSeek").unwrap_or(false),
        track_id: objpath_of(&meta, "mpris:trackid").or_else(|| str_of(&meta, "mpris:trackid")),
    }
}

fn caps_from_props(
    player: &HashMap<String, OwnedValue>,
    root: Option<&HashMap<String, OwnedValue>>,
    playlists: bool,
) -> MediaCaps {
    let control = bool_of(player, "CanControl") == Some(true);
    let volume = control && f64_of(player, "Volume").is_some_and(f64::is_finite);
    MediaCaps {
        shuffle: control && bool_of(player, "Shuffle").is_some(),
        loop_mode: control
            && str_of(player, "LoopStatus")
                .and_then(|s| LoopMode::parse_mpris(&s))
                .is_some(),
        volume,
        playlists,
        signals: true,
        seek: control && bool_of(player, "CanSeek") == Some(true),
        art: true,
        queue: root.and_then(|props| bool_of(props, "HasTrackList")) == Some(true),
        abs_volume: volume,
        chapters: false,
        fullscreen: root.and_then(|props| bool_of(props, "CanSetFullscreen")) == Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_change_between_read_and_write_refuses_without_writing() {
        let writes = std::sync::atomic::AtomicUsize::new(0);
        let result =
            futures::executor::block_on(write_if_same_owner(":1.7", Ok(":1.9".into()), || async {
                writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }));
        assert!(matches!(result, Err(MediaError::Stale(_))));
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn same_owner_writes_once() {
        let writes = std::sync::atomic::AtomicUsize::new(0);
        let result =
            futures::executor::block_on(write_if_same_owner(":1.7", Ok(":1.7".into()), || async {
                writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }));
        assert_eq!(result, Ok(()));
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn owner_lookup_failure_refuses_without_writing() {
        for failure in [
            MediaError::Stale("owner gone".into()),
            MediaError::Unavailable("bus lookup failed".into()),
        ] {
            let writes = std::sync::atomic::AtomicUsize::new(0);
            let result = futures::executor::block_on(write_if_same_owner(
                ":1.7",
                Err(failure.clone()),
                || async {
                    writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            ));
            assert_eq!(result, Err(failure));
            assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn owner_gone_error_names_are_stale() {
        assert!(is_owner_gone_name(
            "org.freedesktop.DBus.Error.ServiceUnknown"
        ));
        assert!(is_owner_gone_name(
            "org.freedesktop.DBus.Error.NameHasNoOwner"
        ));
        assert!(!is_owner_gone_name(
            "org.freedesktop.DBus.Error.UnknownProperty"
        ));
        assert!(!is_owner_gone_name(
            "org.freedesktop.DBus.Error.AccessDenied"
        ));
        assert!(matches!(
            map_mutation_fdo_error(zbus::fdo::Error::NameHasNoOwner("x".into())),
            MediaError::Stale(_)
        ));
        assert!(matches!(
            map_mutation_fdo_error(zbus::fdo::Error::ServiceUnknown("x".into())),
            MediaError::Stale(_)
        ));
        assert!(matches!(
            map_mutation_fdo_error(zbus::fdo::Error::AccessDenied("x".into())),
            MediaError::Backend(_)
        ));
    }

    #[test]
    fn queue_item_membership_requires_exact_tracklist_path() {
        let present = OwnedObjectPath::try_from("/org/mpris/MediaPlayer2/Track/1").unwrap();
        let absent = OwnedObjectPath::try_from("/org/mpris/MediaPlayer2/Track/2").unwrap();
        let tracks = vec![present.clone()];

        for (path, expected) in [(&present, true), (&absent, false)] {
            assert_eq!(queue_item_is_in_tracklist(&tracks, path), expected);
        }
        assert!(!queue_item_is_in_tracklist(&[], &present));
    }

    #[test]
    fn fullscreen_property_uses_root_interface() {
        let seen_iface = std::cell::RefCell::new(None);
        let result = futures::executor::block_on(write_fullscreen_if_same_owner(
            ":1.7",
            Ok(":1.7".into()),
            InterfaceName::try_from(ROOT_IFACE).unwrap(),
            true,
            |iface, value| {
                *seen_iface.borrow_mut() = Some((iface.as_str().to_string(), value));
                async { Ok(()) }
            },
        ));
        assert_eq!(result, Ok(()));
        assert_eq!(
            seen_iface.into_inner(),
            Some((ROOT_IFACE.to_string(), true))
        );
    }

    #[test]
    fn media_error_stale_display() {
        assert_eq!(
            MediaError::Stale("x".into()).to_string(),
            "media player changed: x"
        );
    }

    #[test]
    fn unsigned_media_duration_cannot_wrap_negative() {
        assert_eq!(micros_of(&Value::U64(u64::MAX)), None);
        assert_eq!(micros_of(&Value::U64(i64::MAX as u64)), Some(i64::MAX));
        assert_eq!(micros_of(&Value::U64(30_000_000)), Some(30_000_000));
    }

    #[test]
    fn volume_step_requires_a_read_value() {
        assert!(stepped_volume(None, 0.1).is_err());
        assert!(stepped_volume(Some(f64::NAN), 0.1).is_err());
        assert!(stepped_volume(Some(f64::INFINITY), 0.1).is_err());
        assert_eq!(stepped_volume(Some(0.5), 0.1).unwrap(), 0.6);
        assert_eq!(stepped_volume(Some(0.95), 0.1).unwrap(), 1.0);
        assert_eq!(stepped_volume(Some(0.05), -0.1).unwrap(), 0.0);
    }

    #[test]
    fn fullscreen_toggle_requires_a_read_value() {
        assert!(toggled_fullscreen(None).is_err());
        assert!(toggled_fullscreen(Some(false)).unwrap());
        assert!(!toggled_fullscreen(Some(true)).unwrap());
    }

    #[test]
    fn missing_capability_props_default_to_unsupported() {
        let s = parse_state("p", &HashMap::new());
        assert!(!s.can_go_next);
        assert!(!s.can_go_previous);
        assert!(!s.can_seek);
    }

    fn props(pairs: &[(&str, Value<'_>)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.try_to_owned().unwrap()))
            .collect()
    }

    #[test]
    fn capabilities_require_control_and_report_only_present_features() {
        let player = props(&[("Shuffle", Value::Bool(true)), ("Volume", Value::F64(0.5))]);
        let caps = caps_from_props(&player, None, false);
        assert!(!caps.shuffle);
        assert!(!caps.loop_mode);
        assert!(!caps.volume);
        assert!(!caps.abs_volume);
        assert!(!caps.seek);

        let player = props(&[
            ("CanControl", Value::Bool(true)),
            ("Shuffle", Value::Bool(true)),
            ("Volume", Value::F64(0.5)),
        ]);
        let caps = caps_from_props(&player, None, false);
        assert!(caps.shuffle);
        assert!(!caps.loop_mode);
        assert!(caps.volume);
        assert!(caps.abs_volume);
    }

    #[test]
    fn root_capabilities_fail_closed_without_true_properties() {
        let player = HashMap::new();
        let root = props(&[
            ("HasTrackList", Value::Bool(false)),
            ("CanSetFullscreen", Value::Bool(true)),
        ]);
        let caps = caps_from_props(&player, Some(&root), true);
        assert!(!caps.queue);
        assert!(caps.fullscreen);
        assert!(caps.playlists);

        let caps = caps_from_props(&player, None, false);
        assert!(!caps.queue);
        assert!(!caps.fullscreen);
    }

    #[test]
    fn tail_strips_prefix() {
        assert_eq!(tail("org.mpris.MediaPlayer2.spotify"), "spotify");
        assert_eq!(
            tail("org.mpris.MediaPlayer2.mpv.instance123"),
            "mpv.instance123"
        );
        assert_eq!(tail("weird"), "weird");
    }

    fn cands(pairs: &[(&str, PlaybackState)]) -> Vec<(String, PlaybackState)> {
        pairs.iter().map(|(b, s)| (b.to_string(), *s)).collect()
    }

    const MPV: &str = "org.mpris.MediaPlayer2.mpv.instance42";
    const MPD: &str = "org.mpris.MediaPlayer2.mpd";

    #[test]
    fn choose_priority_beats_a_playing_non_priority_player() {
        // mpd is playing, but the user pinned mpv → mpv wins even though it's paused.
        let c = cands(&[(MPD, PlaybackState::Playing), (MPV, PlaybackState::Paused)]);
        let got = choose_player(&c, &["mpv".to_string()], None);
        assert_eq!(got.as_deref(), Some(MPV));
    }

    #[test]
    fn choose_sticky_retained_while_active_over_another_playing() {
        // Both playing; we were already showing mpv → stay on mpv (no flicker).
        let c = cands(&[(MPD, PlaybackState::Playing), (MPV, PlaybackState::Playing)]);
        let got = choose_player(&c, &[], Some(MPV));
        assert_eq!(got.as_deref(), Some(MPV));
        // Paused counts as active too.
        let c = cands(&[(MPD, PlaybackState::Playing), (MPV, PlaybackState::Paused)]);
        let got = choose_player(&c, &[], Some(MPV));
        assert_eq!(got.as_deref(), Some(MPV));
    }

    #[test]
    fn choose_sticky_dropped_when_stopped_falls_through_to_playing() {
        // Sticky mpv went Stopped; mpd is playing → hand off to mpd.
        let c = cands(&[(MPD, PlaybackState::Playing), (MPV, PlaybackState::Stopped)]);
        let got = choose_player(&c, &[], Some(MPV));
        assert_eq!(got.as_deref(), Some(MPD));
    }

    #[test]
    fn choose_sticky_kept_when_present_but_nothing_playing() {
        // Nothing playing, but sticky mpd is still present → keep it (rule 4),
        // don't jump to the sorted-first mpd/mpv.
        let c = cands(&[(MPD, PlaybackState::Stopped), (MPV, PlaybackState::Paused)]);
        let got = choose_player(&c, &[], Some(MPV));
        assert_eq!(got.as_deref(), Some(MPV));
    }

    #[test]
    fn choose_first_present_when_idle_and_no_sticky() {
        // Deterministic: candidates are sorted, so the first is stable.
        let c = cands(&[(MPD, PlaybackState::Stopped), (MPV, PlaybackState::Stopped)]);
        let got = choose_player(&c, &[], None);
        assert_eq!(got.as_deref(), Some(MPD));
    }

    #[test]
    fn choose_empty_is_none() {
        assert_eq!(choose_player(&[], &["mpv".to_string()], Some(MPV)), None);
    }

    #[test]
    fn unreadable_priority_candidate_is_excluded() {
        let reads = vec![
            (
                MPV.to_string(),
                Err(MediaError::Backend("unreadable".into())),
            ),
            (
                MPD.to_string(),
                Ok(HashMap::from([(
                    "PlaybackStatus".into(),
                    Value::new("Playing").try_to_owned().unwrap(),
                )])),
            ),
        ];
        let candidates = readable_candidates(reads).unwrap();
        assert_eq!(
            choose_player(&candidates, &["mpv".to_string()], None).as_deref(),
            Some(MPD)
        );
    }

    #[test]
    fn all_unreadable_players_return_an_error() {
        let reads = vec![(
            MPV.to_string(),
            Err(MediaError::Backend("unreadable".into())),
        )];
        assert!(matches!(
            readable_candidates(reads),
            Err(MediaError::Backend(_))
        ));
    }

    #[test]
    fn no_mpris_players_remains_an_empty_candidate_list() {
        assert!(readable_candidates(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn parse_state_from_props() {
        // Build a Metadata dict the way MPRIS sends it (variant-wrapped values).
        let mut meta: HashMap<String, OwnedValue> = HashMap::new();
        meta.insert(
            "xesam:title".into(),
            Value::new("Get Lucky").try_to_owned().unwrap(),
        );
        meta.insert(
            "xesam:artist".into(),
            Value::new(vec!["Daft Punk".to_string()])
                .try_to_owned()
                .unwrap(),
        );
        meta.insert(
            "mpris:length".into(),
            Value::new(248_000_000i64).try_to_owned().unwrap(),
        );

        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert(
            "PlaybackStatus".into(),
            Value::new("Playing").try_to_owned().unwrap(),
        );
        props.insert("Metadata".into(), Value::new(meta).try_to_owned().unwrap());
        props.insert("Shuffle".into(), Value::new(true).try_to_owned().unwrap());
        props.insert("Volume".into(), Value::new(0.8f64).try_to_owned().unwrap());

        let s = parse_state("spotify", &props);
        assert_eq!(s.player, "spotify");
        assert_eq!(s.title, "Get Lucky");
        assert_eq!(s.artist, "Daft Punk");
        assert_eq!(s.state, PlaybackState::Playing);
        assert_eq!(s.length, Some(Duration::from_secs(248)));
        assert_eq!(s.shuffle, Some(true));
        assert_eq!(s.volume, Some(80));
        assert_eq!(s.now_playing(), "Daft Punk \u{2014} Get Lucky");
    }

    #[test]
    fn parse_state_peels_variant_wrapped_metadata() {
        // `Properties.GetAll` nests each value in a variant, so `Metadata` can
        // arrive as `v(a{sv})` rather than a bare `a{sv}`. `parse_state` must
        // peel that extra layer, else title/artist come back empty even though
        // the player is happily playing (the real-world "music not detected" bug).
        let mut meta: HashMap<String, OwnedValue> = HashMap::new();
        meta.insert(
            "xesam:title".into(),
            Value::new("The Rebel Path").try_to_owned().unwrap(),
        );
        meta.insert(
            "xesam:artist".into(),
            Value::new(vec!["P.T. Adamczyk".to_string()])
                .try_to_owned()
                .unwrap(),
        );
        // Wrap the metadata dict in an extra variant layer.
        let wrapped = Value::Value(Box::new(Value::new(meta)));

        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert(
            "PlaybackStatus".into(),
            Value::new("Playing").try_to_owned().unwrap(),
        );
        props.insert("Metadata".into(), wrapped.try_to_owned().unwrap());

        let s = parse_state("mpd", &props);
        assert_eq!(s.title, "The Rebel Path");
        assert_eq!(s.artist, "P.T. Adamczyk");
        assert_eq!(s.state, PlaybackState::Playing);
    }
}
