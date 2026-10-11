//! The account's Connect state as Spotify pushes it to the streaming
//! session: which devices there are, what plays on the active one, and what
//! is queued behind it. Reading it takes no request, so it never waits on
//! the shared app's rate limit the way polling the Web API for the same
//! does.
//!
//! A pushed state is believed for [`TRUST`] after the last push, or after
//! a Web API answer that agreed with it; past that, or when a Web API
//! answer disagreed, the Web API is asked instead.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use librespot_core::{Session, SpotifyUri, error::ErrorKind};
use librespot_protocol::connect::Cluster;
use librespot_protocol::devices::DeviceType;

use crate::api::ApiError;
use crate::api::models::{Context, Device, PlayableItem, PlaybackState, Queue};
use crate::session_reads::{self, Failure};

/// How long a pushed state is believed without hearing from Spotify or the
/// Web API again. Spotify pushes on every change, so a quiet account is a
/// still one; the limit only bounds how long a missed push can stay wrong.
const TRUST: Duration = Duration::from_secs(300);
/// How many upcoming songs the queue lists, as the Web API lists them.
const QUEUE_SHOWN: usize = 20;
/// How many songs' details are kept between reads.
const ITEMS_KEPT: usize = 256;

#[derive(Default)]
pub struct ClusterView {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    cluster: Option<Cluster>,
    heard: Option<Instant>,
    items: HashMap<String, PlayableItem>,
}

impl ClusterView {
    /// Takes a state Spotify pushed.
    pub fn update(&self, cluster: Cluster) {
        let mut inner = self.lock();
        inner.cluster = Some(cluster);
        inner.heard = Some(Instant::now());
    }

    /// The state, if one was pushed lately enough to be believed.
    pub fn fresh(&self) -> Option<Cluster> {
        let inner = self.lock();
        let heard = inner.heard?;
        (heard.elapsed() < TRUST).then(|| inner.cluster.clone())?
    }

    /// A Web API answer about the active device and its song, which the
    /// pushed state is held against: agreeing renews it, disagreeing drops
    /// it, so the next push starts it over.
    pub fn check(&self, device: Option<&str>, song: Option<&str>) {
        let mut inner = self.lock();
        let Some(cluster) = inner.cluster.as_ref() else {
            return;
        };
        let pushed_device = Some(cluster.active_device_id.as_str()).filter(|id| !id.is_empty());
        let pushed_song = current_uri(cluster);
        if pushed_device == device && pushed_song == song {
            inner.heard = Some(Instant::now());
        } else {
            inner.cluster = None;
            inner.heard = None;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|lock| lock.into_inner())
    }

    fn known(&self, uri: &str) -> Option<PlayableItem> {
        self.lock().items.get(uri).cloned()
    }

    fn remember(&self, found: HashMap<String, PlayableItem>) {
        let mut inner = self.lock();
        if inner.items.len() + found.len() > ITEMS_KEPT {
            inner.items.clear();
        }
        inner.items.extend(found);
    }
}

/// The devices of the account that can play, the active one first.
pub fn devices(cluster: &Cluster) -> Vec<Device> {
    let mut devices: Vec<Device> = cluster
        .device
        .iter()
        .filter(|(_, info)| info.capabilities.can_be_player && !info.capabilities.hidden)
        .map(|(id, info)| device(cluster, id, info))
        .collect();
    devices.sort_by(|a, b| {
        b.is_active
            .cmp(&a.is_active)
            .then_with(|| a.name.cmp(&b.name))
    });
    devices
}

fn device(cluster: &Cluster, id: &str, info: &librespot_protocol::connect::DeviceInfo) -> Device {
    Device {
        id: Some(id.to_string()),
        name: info.name.clone(),
        is_active: !cluster.active_device_id.is_empty() && cluster.active_device_id == id,
        is_restricted: false,
        volume_percent: Some(
            ((u64::from(info.volume) * 100 + 32_767) / 65_535).min(100) as u8,
        ),
        supports_volume: Some(!info.capabilities.disable_volume),
        kind: device_kind(info.device_type.enum_value().unwrap_or(DeviceType::UNKNOWN))
            .to_string(),
    }
}

/// The icon names the Web API gives a device.
fn device_kind(kind: DeviceType) -> &'static str {
    match kind {
        DeviceType::COMPUTER | DeviceType::CHROMEBOOK => "Computer",
        DeviceType::TABLET => "Tablet",
        DeviceType::SMARTPHONE | DeviceType::SMARTWATCH => "Smartphone",
        DeviceType::SPEAKER | DeviceType::HOME_THING => "Speaker",
        DeviceType::TV => "TV",
        DeviceType::AVR => "AVR",
        DeviceType::STB => "STB",
        DeviceType::AUDIO_DONGLE => "AudioDongle",
        DeviceType::GAME_CONSOLE => "GameConsole",
        DeviceType::CAST_VIDEO => "CastVideo",
        DeviceType::CAST_AUDIO => "CastAudio",
        DeviceType::AUTOMOBILE | DeviceType::CAR_THING => "Automobile",
        _ => "Unknown",
    }
}

/// The song or episode on the active device.
pub fn current_uri(cluster: &Cluster) -> Option<&str> {
    if cluster.active_device_id.is_empty() {
        return None;
    }
    Some(cluster.player_state.track.uri.as_str()).filter(|uri| !uri.is_empty())
}

/// What is queued behind the current song, in play order, as the Web API
/// lists it: the markers that divide the queue from the context are left
/// out.
pub fn upcoming(cluster: &Cluster) -> Vec<&str> {
    cluster
        .player_state
        .next_tracks
        .iter()
        .map(|track| track.uri.as_str())
        .filter(|uri| uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:"))
        .take(QUEUE_SHOWN)
        .collect()
}

/// The playback state the Web API would answer with, `None` where it
/// answers that nothing is playing anywhere.
pub fn playback_state(
    cluster: &Cluster,
    item: Option<PlayableItem>,
    now_ms: u64,
) -> Option<PlaybackState> {
    let uri = current_uri(cluster)?;
    let active = devices(cluster).into_iter().find(|device| device.is_active)?;
    let state = &cluster.player_state;
    let playing = state.is_playing && !state.is_paused;
    let duration = u64::try_from(state.duration).unwrap_or_default();
    let mut progress = u64::try_from(state.position_as_of_timestamp).unwrap_or_default();
    if playing && state.timestamp > 0 {
        progress += now_ms.saturating_sub(state.timestamp as u64);
    }
    if duration > 0 {
        progress = progress.min(duration);
    }
    let options = &state.options;
    let repeat = if options.repeating_track {
        "track"
    } else if options.repeating_context {
        "context"
    } else {
        "off"
    };
    Some(PlaybackState {
        device: Some(active),
        repeat_state: repeat.to_string(),
        shuffle_state: options.shuffling_context,
        context: Some(state.context_uri.as_str())
            .filter(|context| !context.is_empty())
            .map(|context| Context {
                uri: context.to_string(),
                kind: context_kind(context),
            }),
        timestamp: now_ms,
        progress_ms: Some(u32::try_from(progress).unwrap_or(u32::MAX)),
        is_playing: playing,
        item,
        currently_playing_type: Some(playing_type(uri).to_string()),
    })
}

/// The kind of thing a context is, as the Web API names it.
fn context_kind(uri: &str) -> String {
    let parts: Vec<&str> = uri.split(':').collect();
    match parts.as_slice() {
        ["spotify", "user", _, "collection", ..] | ["spotify", "collection", ..] => "collection",
        ["spotify", "user", _, "playlist", _] => "playlist",
        ["spotify", kind, ..] => kind,
        _ => "",
    }
    .to_string()
}

fn playing_type(uri: &str) -> &'static str {
    if uri.starts_with("spotify:track:") {
        "track"
    } else if uri.starts_with("spotify:episode:") {
        "episode"
    } else if uri.starts_with("spotify:ad:") {
        "ad"
    } else {
        "unknown"
    }
}

/// The song playing and the queue behind it, from the details in `found`;
/// an entry Spotify has no details for is left out.
pub fn queue(cluster: &Cluster, found: &HashMap<String, PlayableItem>) -> Queue {
    Queue {
        currently_playing: current_uri(cluster).and_then(|uri| found.get(uri).cloned()),
        queue: upcoming(cluster)
            .into_iter()
            .filter_map(|uri| found.get(uri).cloned())
            .collect(),
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The details of `uris`, from what was read before or one batched request
/// for the rest. An entry Spotify has no details for is absent.
pub async fn details(
    session: &Session,
    view: &ClusterView,
    uris: &[&str],
) -> Result<HashMap<String, PlayableItem>, Failure> {
    let mut found = HashMap::new();
    let mut missing = Vec::new();
    for uri in uris {
        match view.known(uri) {
            Some(item) => {
                found.insert((*uri).to_string(), item);
            }
            None => {
                if let Ok(parsed) = SpotifyUri::from_uri(uri) {
                    missing.push(parsed);
                }
            }
        }
    }
    if !missing.is_empty() {
        let fetched = session_reads::playables(session, &missing).await?;
        view.remember(fetched.clone());
        found.extend(fetched);
    }
    Ok(found)
}

/// The Web API's playback state, from the pushed one.
pub async fn playback(
    session: &Session,
    view: &ClusterView,
    cluster: &Cluster,
) -> Result<Option<PlaybackState>, Failure> {
    let Some(uri) = current_uri(cluster) else {
        return Ok(None);
    };
    if uri.starts_with("spotify:local:") {
        return Err(Failure::Retry(anyhow::anyhow!(
            "a local file is the Web API's to describe"
        )));
    }
    let mut found = details(session, view, &[uri]).await?;
    Ok(playback_state(cluster, found.remove(uri), now_ms()))
}

/// The Web API's queue, from the pushed state.
pub async fn upcoming_queue(
    session: &Session,
    view: &ClusterView,
    cluster: &Cluster,
) -> Result<Queue, Failure> {
    let mut uris: Vec<&str> = current_uri(cluster).into_iter().collect();
    uris.extend(upcoming(cluster));
    let found = details(session, view, &uris).await?;
    Ok(queue(cluster, &found))
}


/// What the account's other devices are told to do, over the same channel
/// Spotify's own clients use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Resume,
    Pause,
    Next,
    Previous,
    Seek(u32),
    /// A level from 0 to 100.
    Volume(u8),
    Shuffle(bool),
    /// `off`, `context` or `track`, as the Web API names them.
    Repeat(&'a str),
    Queue(&'a str),
    /// Playback moves to the device; `play` asks it to be playing there.
    Transfer {
        play: bool,
    },
}

/// What a command is sent as: the path under the connect-state service,
/// the verb, and the JSON body.
fn request(command: Command<'_>, from: &str, to: &str, id: &str) -> (String, bool, serde_json::Value) {
    use serde_json::json;

    let player = format!("/connect-state/v1/player/command/from/{from}/to/{to}");
    let logging = json!({"command_id": id});
    let endpoint = |endpoint: &str, extra: serde_json::Value| {
        let mut command = json!({"endpoint": endpoint, "logging_params": logging});
        if let (Some(command), Some(extra)) = (command.as_object_mut(), extra.as_object()) {
            command.extend(extra.clone());
        }
        (player.clone(), false, json!({"command": command}))
    };
    match command {
        Command::Resume => endpoint("resume", json!({})),
        Command::Pause => endpoint("pause", json!({})),
        Command::Next => endpoint("skip_next", json!({})),
        Command::Previous => endpoint("skip_prev", json!({})),
        Command::Seek(ms) => endpoint("seek_to", json!({"value": ms, "position": ms})),
        Command::Shuffle(on) => endpoint("set_shuffling_context", json!({"value": on})),
        Command::Repeat(mode) => endpoint(
            "set_options",
            json!({
                "repeating_context": mode == "context",
                "repeating_track": mode == "track",
            }),
        ),
        Command::Queue(uri) => endpoint(
            "add_to_queue",
            json!({"track": {"uri": uri, "metadata": {"is_queued": "true"}, "provider": "queue"}}),
        ),
        Command::Volume(percent) => (
            format!("/connect-state/v1/connect/volume/from/{from}/to/{to}"),
            true,
            json!({"volume": (u32::from(percent.min(100)) * 65_535 + 50) / 100}),
        ),
        // Replaced by the transfer endpoint, which takes no body here.
        Command::Transfer { .. } => (String::new(), false, serde_json::Value::Null),
    }
}

/// Sends `command` to `target`, or to the active device when there is no
/// target, from this session's own device. A refusal Spotify gave before
/// doing anything is a retry, left to the Web API; a command that may have
/// gone out and was not confirmed is final, so it is never sent twice.
pub async fn send(
    session: &Session,
    view: &ClusterView,
    own: &str,
    target: Option<&str>,
    command: Command<'_>,
) -> Result<(), Failure> {
    let retry = |reason: &str| Failure::Retry(anyhow::anyhow!("command: {reason}"));
    let cluster = view.fresh().ok_or_else(|| retry("no state pushed"))?;
    let target = target
        .map(str::to_string)
        .or_else(|| Some(cluster.active_device_id.clone()).filter(|id| !id.is_empty()))
        .ok_or_else(|| retry("no active device"))?;
    if !cluster.device.contains_key(&target) {
        return Err(retry("device not in the account's list"));
    }
    if let Command::Transfer { play } = command {
        let playing = cluster.player_state.is_playing && !cluster.player_state.is_paused;
        if play && !playing {
            return Err(retry("starting playback on transfer is the Web API's"));
        }
        return session
            .spclient()
            .transfer(own, &target, None)
            .await
            .map(drop)
            .map_err(refusal);
    }
    let (path, put, body) = request(command, own, &target, &command_id());
    let method = if put {
        http::Method::PUT
    } else {
        http::Method::POST
    };
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    session
        .spclient()
        .request(&method, &path, Some(headers), Some(body.to_string().as_bytes()))
        .await
        .map(drop)
        .map_err(refusal)
}

fn refusal(error: librespot_core::Error) -> Failure {
    match error.kind {
        ErrorKind::NotFound
        | ErrorKind::PermissionDenied
        | ErrorKind::InvalidArgument
        | ErrorKind::FailedPrecondition
        | ErrorKind::Unauthenticated
        | ErrorKind::ResourceExhausted
        | ErrorKind::Unimplemented => Failure::Retry(error.into()),
        _ => Failure::Definitive(ApiError::Status {
            status: 502,
            message: "Spotify did not confirm the command".into(),
        }),
    }
}

/// An id for one command, as the clients' own logging names it.
fn command_id() -> String {
    format!("{:032x}", std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos()))
}

#[cfg(test)]
mod tests {
    use librespot_protocol::connect::{Capabilities, DeviceInfo};
    use librespot_protocol::player::{ContextPlayerOptions, PlayerState, ProvidedTrack};
    use protobuf::{EnumOrUnknown, MessageField};

    use super::*;
    use crate::api::models::Track;

    fn info(name: &str, kind: DeviceType, volume: u32, player: bool) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            volume,
            device_type: EnumOrUnknown::new(kind),
            capabilities: MessageField::some(Capabilities {
                can_be_player: player,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn provided(uri: &str) -> ProvidedTrack {
        ProvidedTrack {
            uri: uri.to_string(),
            ..Default::default()
        }
    }

    fn cluster() -> Cluster {
        let mut cluster = Cluster {
            active_device_id: "phone".to_string(),
            player_state: MessageField::some(PlayerState {
                context_uri: "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".to_string(),
                track: MessageField::some(provided("spotify:track:4uLU6hMCjMI75M1A2tKUQC")),
                is_playing: true,
                is_paused: false,
                position_as_of_timestamp: 10_000,
                timestamp: 1_000_000,
                duration: 200_000,
                options: MessageField::some(ContextPlayerOptions {
                    shuffling_context: true,
                    repeating_context: true,
                    ..Default::default()
                }),
                next_tracks: vec![
                    provided("spotify:track:0000000000000000000001"),
                    provided("spotify:delimiter"),
                    provided("spotify:episode:7GhIk7Il098yCjg4BQjzvb"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        cluster.device.insert(
            "phone".into(),
            info("Pixel", DeviceType::SMARTPHONE, 32_768, true),
        );
        cluster.device.insert(
            "pc".into(),
            info("Spotifast", DeviceType::COMPUTER, 65_535, true),
        );
        cluster.device.insert(
            "controller".into(),
            info("Remote", DeviceType::SMARTPHONE, 0, false),
        );
        cluster
    }

    #[test]
    fn devices_list_players_with_the_active_one_first() {
        let list = devices(&cluster());
        let names: Vec<_> = list.iter().map(|device| device.name.as_str()).collect();
        assert_eq!(names, ["Pixel", "Spotifast"]);
        assert!(list[0].is_active && !list[1].is_active);
        assert_eq!(list[0].kind, "Smartphone");
        assert_eq!(list[0].volume_percent, Some(50));
        assert_eq!(list[1].volume_percent, Some(100));
        assert_eq!(list[1].id.as_deref(), Some("pc"));
    }

    #[test]
    fn playback_carries_position_options_and_context() {
        let state = playback_state(&cluster(), None, 1_005_000).unwrap();
        assert!(state.is_playing);
        assert_eq!(state.progress_ms, Some(15_000));
        assert_eq!(state.repeat_state, "context");
        assert!(state.shuffle_state);
        let context = state.context.unwrap();
        assert_eq!((context.kind.as_str(), context.uri.as_str()), (
            "playlist",
            "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"
        ));
        assert_eq!(state.device.unwrap().name, "Pixel");
        assert_eq!(state.currently_playing_type.as_deref(), Some("track"));
    }

    #[test]
    fn a_paused_song_stays_where_it_stopped_and_never_passes_its_end() {
        let mut cluster = cluster();
        cluster.player_state.mut_or_insert_default().is_paused = true;
        let state = playback_state(&cluster, None, 9_000_000).unwrap();
        assert!(!state.is_playing);
        assert_eq!(state.progress_ms, Some(10_000));
        cluster.player_state.mut_or_insert_default().is_paused = false;
        let state = playback_state(&cluster, None, 9_000_000).unwrap();
        assert_eq!(state.progress_ms, Some(200_000));
    }

    #[test]
    fn nothing_plays_without_an_active_device_or_a_song() {
        let mut idle = cluster();
        idle.active_device_id.clear();
        assert!(playback_state(&idle, None, 0).is_none());
        let mut empty = cluster();
        empty.player_state.mut_or_insert_default().track = MessageField::some(provided(""));
        assert!(playback_state(&empty, None, 0).is_none());
    }

    #[test]
    fn the_queue_skips_markers_and_keeps_order() {
        let cluster = cluster();
        assert_eq!(
            upcoming(&cluster),
            [
                "spotify:track:0000000000000000000001",
                "spotify:episode:7GhIk7Il098yCjg4BQjzvb"
            ]
        );
        let song = |uri: &str| {
            PlayableItem::Track(Track {
                uri: uri.to_string(),
                ..Default::default()
            })
        };
        let mut found = HashMap::new();
        for uri in [
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
            "spotify:track:0000000000000000000001",
        ] {
            found.insert(uri.to_string(), song(uri));
        }
        let queue = queue(&cluster, &found);
        assert!(queue.currently_playing.is_some());
        // The episode has no details and is left out, not shown empty.
        assert_eq!(queue.queue.len(), 1);
    }

    #[test]
    fn a_disagreeing_answer_drops_the_state_and_an_agreeing_one_keeps_it() {
        let view = ClusterView::default();
        assert!(view.fresh().is_none());
        view.update(cluster());
        assert!(view.fresh().is_some());
        view.check(Some("phone"), Some("spotify:track:4uLU6hMCjMI75M1A2tKUQC"));
        assert!(view.fresh().is_some());
        view.check(Some("pc"), Some("spotify:track:4uLU6hMCjMI75M1A2tKUQC"));
        assert!(view.fresh().is_none());
    }

    #[test]
    fn contexts_name_their_kind_as_the_web_api_does() {
        assert_eq!(context_kind("spotify:album:abc"), "album");
        assert_eq!(context_kind("spotify:user:me:collection"), "collection");
        assert_eq!(context_kind("spotify:user:me:playlist:abc"), "playlist");
        assert_eq!(context_kind("spotify:show:abc"), "show");
    }

    #[test]
    fn commands_go_out_in_the_clients_own_shape() {
        let (path, put, body) = request(Command::Seek(1500), "me", "pc", "abc");
        assert_eq!(path, "/connect-state/v1/player/command/from/me/to/pc");
        assert!(!put);
        assert_eq!(body["command"]["endpoint"], "seek_to");
        assert_eq!(body["command"]["value"], 1500);
        assert_eq!(body["command"]["logging_params"]["command_id"], "abc");

        let (_, _, body) = request(Command::Repeat("track"), "me", "pc", "abc");
        assert_eq!(body["command"]["endpoint"], "set_options");
        assert_eq!(body["command"]["repeating_track"], true);
        assert_eq!(body["command"]["repeating_context"], false);

        let (_, _, body) = request(Command::Queue("spotify:track:x"), "me", "pc", "abc");
        assert_eq!(body["command"]["track"]["uri"], "spotify:track:x");

        let (path, put, body) = request(Command::Volume(50), "me", "pc", "abc");
        assert_eq!(path, "/connect-state/v1/connect/volume/from/me/to/pc");
        assert!(put);
        assert_eq!(body["volume"], 32_768);
    }
}
