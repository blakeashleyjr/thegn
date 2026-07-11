//! Native MPRIS backend over D-Bus (`zbus`). Implements [`MediaBackend`] against
//! the `org.mpris.MediaPlayer2{,.Player,.Playlists}` interfaces every compliant
//! player exposes, plus a push [`MprisWatch`] built from D-Bus signals so the
//! host updates on change without polling.

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use zbus::message::Type as MsgType;
use zbus::names::InterfaceName;
use zbus::zvariant::{Array, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use crate::model::{LoopMode, MediaKind, MediaState, PlaybackState, Playlist, QueueItem};
use crate::{MediaBackend, MediaCaps, MediaError, MediaWatch};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const PLAYLISTS_IFACE: &str = "org.mpris.MediaPlayer2.Playlists";
const TRACKLIST_IFACE: &str = "org.mpris.MediaPlayer2.TrackList";

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
        let mut candidates: Vec<(String, PlaybackState)> = Vec::with_capacity(names.len());
        for n in names {
            let state = match self.player_props(&n).await {
                Ok(props) => playback_state(&props),
                // A player that won't answer `GetAll` still counts as present
                // (Stopped) so priority/sticky can target it if nothing better.
                Err(_) => PlaybackState::Stopped,
            };
            candidates.push((n, state));
        }
        let sticky = self.last.lock().ok().and_then(|g| g.clone());
        let chosen = choose_player(&candidates, &self.priority, sticky.as_deref());
        if let Ok(mut last) = self.last.lock() {
            *last = chosen.clone();
        }
        Ok(chosen)
    }

    /// Invoke a no-argument `Player` method (PlayPause / Next / Previous).
    async fn player_call(&self, bus: &str, member: &str) -> Result<(), MediaError> {
        self.conn
            .call_method(Some(bus), MPRIS_PATH, Some(PLAYER_IFACE), member, &())
            .await
            .map(|_| ())
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    /// Set a `Player` property.
    async fn player_set(&self, bus: &str, prop: &str, value: Value<'_>) -> Result<(), MediaError> {
        let proxy = self.props_proxy(bus).await?;
        let iface = InterfaceName::try_from(PLAYER_IFACE)
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        proxy
            .set(iface, prop, value)
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))
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
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        let Some(bus) = self.active_player().await? else {
            tracing::debug!(target: "thegn::media", "MPRIS: no active player on the bus");
            return Ok(None);
        };
        let props = self.player_props(&bus).await?;
        let state = parse_state(tail(&bus), &props);
        tracing::debug!(
            target: "thegn::media",
            bus = %bus, state = ?state.state, title = %state.title, artist = %state.artist,
            "MPRIS snapshot",
        );
        Ok(Some(state))
    }

    async fn play_pause(&self) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        self.player_call(&bus, "PlayPause").await
    }
    async fn next(&self) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        self.player_call(&bus, "Next").await
    }
    async fn previous(&self) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        self.player_call(&bus, "Previous").await
    }
    async fn set_shuffle(&self, on: bool) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        self.player_set(&bus, "Shuffle", Value::Bool(on)).await
    }
    async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        self.player_set(&bus, "LoopStatus", Value::from(mode.as_mpris()))
            .await
    }
    async fn volume_step(&self, delta: f64) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        let props = self.player_props(&bus).await?;
        let cur = f64_of(&props, "Volume").unwrap_or(0.5);
        let next = (cur + delta).clamp(0.0, 1.0);
        self.player_set(&bus, "Volume", Value::F64(next)).await
    }

    async fn playlists(&self) -> Result<Vec<Playlist>, MediaError> {
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
    }

    async fn activate_playlist(&self, id: &str) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        let path = OwnedObjectPath::try_from(id).map_err(|e| MediaError::Backend(e.to_string()))?;
        self.conn
            .call_method(
                Some(bus.as_str()),
                MPRIS_PATH,
                Some(PLAYLISTS_IFACE),
                "ActivatePlaylist",
                &(path,),
            )
            .await
            .map(|_| ())
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    async fn seek(&self, offset: Duration, forward: bool) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        let micros = offset.as_micros().min(i64::MAX as u128) as i64;
        let signed = if forward { micros } else { -micros };
        // Player.Seek(Offset: x)
        self.conn
            .call_method(
                Some(bus.as_str()),
                MPRIS_PATH,
                Some(PLAYER_IFACE),
                "Seek",
                &(signed,),
            )
            .await
            .map(|_| ())
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    async fn set_position(&self, pos: Duration, track_id: Option<&str>) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        // Resolve the track id: prefer the caller's, else read it fresh.
        let tid = match track_id {
            Some(t) => t.to_string(),
            None => {
                let props = self.player_props(&bus).await?;
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
                Some(bus.as_str()),
                MPRIS_PATH,
                Some(PLAYER_IFACE),
                "SetPosition",
                &(path, micros),
            )
            .await
            .map(|_| ())
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    async fn set_volume(&self, level: u8) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        let v = (level.min(100) as f64) / 100.0;
        self.player_set(&bus, "Volume", Value::F64(v)).await
    }

    async fn queue(&self) -> Result<Vec<QueueItem>, MediaError> {
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
    }

    async fn play_queue_item(&self, id: &str) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        let path = OwnedObjectPath::try_from(id).map_err(|e| MediaError::Backend(e.to_string()))?;
        // TrackList.GoTo(TrackId: o)
        self.conn
            .call_method(
                Some(bus.as_str()),
                MPRIS_PATH,
                Some(TRACKLIST_IFACE),
                "GoTo",
                &(path,),
            )
            .await
            .map(|_| ())
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    async fn toggle_fullscreen(&self) -> Result<(), MediaError> {
        let bus = self.active_player().await?.ok_or(MediaError::NoPlayer)?;
        // Root MediaPlayer2.Fullscreen (writable when CanSetFullscreen); read the
        // current value and set its inverse so the op is a self-contained toggle.
        let proxy = self.props_proxy(&bus).await?;
        let iface =
            InterfaceName::try_from(ROOT_IFACE).map_err(|e| MediaError::Backend(e.to_string()))?;
        let cur = proxy
            .get(iface.clone(), "Fullscreen")
            .await
            .ok()
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        proxy
            .set(iface, "Fullscreen", Value::Bool(!cur))
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))
    }

    fn caps(&self) -> MediaCaps {
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
        Value::U64(n) => Some(*n as i64),
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
        loop_mode: str_of(props, "LoopStatus").map(|s| LoopMode::from_mpris(&s)),
        volume: f64_of(props, "Volume").map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u8),
        can_go_next: bool_of(props, "CanGoNext").unwrap_or(true),
        can_go_previous: bool_of(props, "CanGoPrevious").unwrap_or(true),
        art_url: str_of(&meta, "mpris:artUrl").filter(|s| !s.is_empty()),
        kind: MediaKind::from_hints(player, mime.as_deref(), url.as_deref()),
        can_seek: bool_of(props, "CanSeek").unwrap_or(false),
        track_id: objpath_of(&meta, "mpris:trackid").or_else(|| str_of(&meta, "mpris:trackid")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
