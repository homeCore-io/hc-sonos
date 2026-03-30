//! Per-speaker state polling and command execution.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use sonor::{RepeatMode, Speaker};
use tracing::warn;

use crate::api::content;
use crate::events::{AvtState, RcState};

fn supported_actions() -> Vec<&'static str> {
    vec![
        "play",
        "pause",
        "stop",
        "next",
        "previous",
        "set_volume",
        "set_mute",
        "seek",
        "play_media",
        "join",
        "unjoin",
        "set_shuffle",
        "set_repeat",
        "set_bass",
        "set_treble",
        "set_loudness",
    ]
}

fn ui_enrichments() -> Vec<&'static str> {
    vec!["favorites", "playlists", "grouping", "audio_eq"]
}

fn repeat_to_str(r: RepeatMode) -> &'static str {
    match r {
        RepeatMode::None => "none",
        RepeatMode::One => "one",
        RepeatMode::All => "all",
    }
}

fn str_to_repeat(s: &str) -> RepeatMode {
    match s {
        "one" => RepeatMode::One,
        "all" => RepeatMode::All,
        _ => RepeatMode::None,
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Snapshot of a speaker's state used for change detection.
/// `repeat` is stored as a string ("none" | "one" | "all") to avoid
/// depending on `RepeatMode`'s trait implementations.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerState {
    pub playing: bool,
    pub volume: u16,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<u32>,
    pub position: Option<u32>,
    pub bass: i8,
    pub treble: i8,
    pub loudness: bool,
    /// Populated by bridge after zone_group_state() query.
    pub group_coordinator: Option<String>,
    pub group_members: Vec<String>,
    pub available_favorites: Vec<String>,
    pub available_playlists: Vec<String>,
}

impl Default for SpeakerState {
    fn default() -> Self {
        Self {
            playing: false,
            volume: 0,
            muted: false,
            shuffle: false,
            repeat: "none".to_string(),
            title: None,
            artist: None,
            album: None,
            duration: None,
            position: None,
            bass: 0,
            treble: 0,
            loudness: false,
            group_coordinator: None,
            group_members: vec![],
            available_favorites: vec![],
            available_playlists: vec![],
        }
    }
}

impl SpeakerState {
    /// Apply a partial AVTransport update in-place.
    pub fn apply_avt(&mut self, avt: &AvtState) {
        if let Some(v) = avt.playing {
            self.playing = v;
        }
        if let Some(v) = avt.shuffle {
            self.shuffle = v;
        }
        if let Some(ref v) = avt.repeat {
            self.repeat = v.clone();
        }
        if let Some(v) = avt.duration {
            self.duration = Some(v);
        }
        if let Some(v) = avt.position {
            self.position = Some(v);
        }
        if avt.track_info_present {
            self.title = avt.title.clone();
            self.artist = avt.artist.clone();
            self.album = avt.album.clone();
        }
    }

    /// Apply a partial RenderingControl update in-place.
    pub fn apply_rc(&mut self, rc: &RcState) {
        if let Some(v) = rc.volume {
            self.volume = v;
        }
        if let Some(v) = rc.muted {
            self.muted = v;
        }
        if let Some(v) = rc.bass {
            self.bass = v;
        }
        if let Some(v) = rc.treble {
            self.treble = v;
        }
        if let Some(v) = rc.loudness {
            self.loudness = v;
        }
    }
}

/// Poll all state from a speaker in one pass.
pub async fn poll(speaker: &Speaker) -> Result<SpeakerState> {
    let playing = speaker.is_playing().await?;
    let volume = speaker.volume().await?;
    let muted = speaker.mute().await?;
    let shuffle = speaker.shuffle().await?;
    let repeat = repeat_to_str(speaker.repeat_mode().await?).to_string();
    let bass = speaker.bass().await?;
    let treble = speaker.treble().await?;
    let loudness = speaker.loudness().await?;

    let (title, artist, album, duration, position) = match speaker.track().await? {
        Some(info) => {
            let t = info.track();
            (
                Some(t.title().to_string()),
                t.creator().map(str::to_string),
                t.album().map(str::to_string),
                Some(info.duration()),
                Some(info.elapsed()),
            )
        }
        None => (None, None, None, None, None),
    };

    Ok(SpeakerState {
        playing,
        volume,
        muted,
        shuffle,
        repeat,
        title,
        artist,
        album,
        duration,
        position,
        bass,
        treble,
        loudness,
        group_coordinator: None,
        group_members: vec![],
        available_favorites: vec![],
        available_playlists: vec![],
    })
}

/// Serialise a `SpeakerState` to the HomeCore JSON state schema.
pub fn to_json(state: &SpeakerState) -> Value {
    let transport = if state.playing { "playing" } else { "paused" };
    let supported_actions = supported_actions();
    let ui_enrichments = ui_enrichments();

    let mut obj = json!({
        "state":    transport,
        "volume":   state.volume,
        "muted":    state.muted,
        "supported_actions": supported_actions,
        "ui_enrichments": ui_enrichments,
        "shuffle":  state.shuffle,
        "repeat":   state.repeat,
        "bass":     state.bass,
        "treble":   state.treble,
        "loudness": state.loudness,
        "group_coordinator": state.group_coordinator,
        "group_members":     state.group_members,
        "available_favorites": state.available_favorites,
        "available_playlists": state.available_playlists,
        "sonos": {
            "favorites": state.available_favorites,
            "playlists": state.available_playlists,
            "group_coordinator": state.group_coordinator,
            "group_members": state.group_members,
        },
    });

    if let Some(v) = &state.title {
        obj["title"] = json!(v);
        obj["media_title"] = json!(v);
    }
    if let Some(v) = &state.artist {
        obj["artist"] = json!(v);
        obj["media_artist"] = json!(v);
    }
    if let Some(v) = &state.album {
        obj["album"] = json!(v);
        obj["media_album"] = json!(v);
    }
    if let Some(v) = state.duration {
        obj["duration_secs"] = json!(v);
        obj["media_duration"] = json!(v);
    }
    if let Some(v) = state.position {
        obj["position_secs"] = json!(v);
        obj["media_position"] = json!(v);
    }

    obj
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

/// Execute a HomeCore command on a speaker.
///
/// `uuid_to_room` maps speaker UUID → room name and is required for the
/// `join` command (sonor's `join()` takes a room name, not a UUID).
pub async fn execute_command(
    speaker: &Speaker,
    cmd: &Value,
    uuid_to_room: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let action = cmd["action"].as_str().unwrap_or("");

    match action {
        "play" => speaker.play().await?,
        "pause" => speaker.pause().await?,
        "stop" => speaker.stop().await?,
        "toggle_play_pause" => {
            if speaker.is_playing().await? {
                speaker.pause().await?;
            } else {
                speaker.play().await?;
            }
        }
        "next" => speaker.next().await?,
        "previous" => speaker.previous().await?,

        "set_volume" => {
            let vol = cmd["volume"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("set_volume requires integer 'volume'"))?;
            speaker.set_volume(vol as u16).await?;
        }

        "mute" | "set_mute" => {
            let muted = cmd["muted"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("set_mute requires boolean 'muted'"))?;
            speaker.set_mute(muted).await?;
        }

        "seek" => {
            let secs = cmd["position"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("seek requires integer 'position'"))?;
            speaker.skip_to(secs as u32).await?;
        }

        "set_shuffle" => {
            let shuffle = cmd["shuffle"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("set_shuffle requires boolean 'shuffle'"))?;
            speaker.set_shuffle(shuffle).await?;
        }

        "set_repeat" => {
            let mode = str_to_repeat(cmd["repeat"].as_str().unwrap_or("none"));
            speaker.set_repeat_mode(mode).await?;
        }

        "set_bass" => {
            let bass = cmd["bass"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("set_bass requires integer 'bass'"))?;
            speaker.set_bass(bass as i8).await?;
        }

        "set_treble" => {
            let treble = cmd["treble"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("set_treble requires integer 'treble'"))?;
            speaker.set_treble(treble as i8).await?;
        }

        "set_loudness" => {
            let loudness = cmd["loudness"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("set_loudness requires boolean 'loudness'"))?;
            speaker.set_loudness(loudness).await?;
        }

        "play_uri" => {
            let uri = cmd["uri"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("play_uri requires string 'uri'"))?;
            let metadata = cmd["metadata"].as_str().unwrap_or("");
            speaker.set_transport_uri(uri, metadata).await?;
            speaker.play().await?;
        }

        "play_favorite" => {
            let target = cmd["favorite"]
                .as_str()
                .or_else(|| cmd["name"].as_str())
                .ok_or_else(|| anyhow::anyhow!("play_favorite requires string 'favorite'"))?;
            let Some((uri, metadata)) = content::get_favorite_by_name(speaker, target).await?
            else {
                bail!("unknown Sonos favorite: {target}");
            };
            speaker.set_transport_uri(&uri, &metadata).await?;
            speaker.play().await?;
        }

        "play_playlist" => {
            let target = cmd["playlist"]
                .as_str()
                .or_else(|| cmd["name"].as_str())
                .ok_or_else(|| anyhow::anyhow!("play_playlist requires string 'playlist'"))?;
            let Some((uri, metadata)) = content::get_playlist_by_name(speaker, target).await?
            else {
                bail!("unknown Sonos playlist: {target}");
            };
            speaker.set_transport_uri(&uri, &metadata).await?;
            speaker.play().await?;
        }

        "play_media" => {
            let media_type = cmd["media_type"].as_str().unwrap_or("");
            match media_type {
                "favorite" => {
                    let target = cmd["name"]
                        .as_str()
                        .or_else(|| cmd["favorite"].as_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!("play_media favorite requires string 'name'")
                        })?;
                    let Some((uri, metadata)) =
                        content::get_favorite_by_name(speaker, target).await?
                    else {
                        bail!("unknown Sonos favorite: {target}");
                    };
                    speaker.set_transport_uri(&uri, &metadata).await?;
                    speaker.play().await?;
                }
                "playlist" => {
                    let target = cmd["name"]
                        .as_str()
                        .or_else(|| cmd["playlist"].as_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!("play_media playlist requires string 'name'")
                        })?;
                    let Some((uri, metadata)) =
                        content::get_playlist_by_name(speaker, target).await?
                    else {
                        bail!("unknown Sonos playlist: {target}");
                    };
                    speaker.set_transport_uri(&uri, &metadata).await?;
                    speaker.play().await?;
                }
                "uri" => {
                    let uri = cmd["uri"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("play_media uri requires string 'uri'"))?;
                    let metadata = cmd["metadata"].as_str().unwrap_or("");
                    speaker.set_transport_uri(uri, metadata).await?;
                    speaker.play().await?;
                }
                other => bail!("unsupported play_media type: {other}"),
            }
        }

        "join" => {
            let coordinator_uuid = cmd["coordinator"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("join requires string 'coordinator' (UUID)"))?;
            let room_name = uuid_to_room
                .get(coordinator_uuid)
                .ok_or_else(|| anyhow::anyhow!("unknown coordinator UUID: {coordinator_uuid}"))?;
            let joined = speaker.join(room_name).await?;
            if !joined {
                warn!(coordinator = %room_name, "join() returned false — speaker may already be in group");
            }
        }

        "unjoin" => {
            speaker.leave().await?;
        }

        other => bail!("unknown action: {other}"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{to_json, SpeakerState};

    #[test]
    fn to_json_publishes_generic_media_contract_and_sonos_enrichments() {
        let state = SpeakerState {
            playing: true,
            volume: 27,
            muted: false,
            shuffle: true,
            repeat: "all".to_string(),
            title: Some("Track Title".to_string()),
            artist: Some("Artist Name".to_string()),
            album: Some("Album Name".to_string()),
            duration: Some(240),
            position: Some(45),
            bass: 3,
            treble: -1,
            loudness: true,
            group_coordinator: Some("media.living_room".to_string()),
            group_members: vec!["media.living_room".to_string(), "media.kitchen".to_string()],
            available_favorites: vec!["Jazz".to_string(), "News".to_string()],
            available_playlists: vec!["Morning".to_string()],
        };

        let json = to_json(&state);

        assert_eq!(json["state"].as_str(), Some("playing"));
        assert_eq!(json["title"].as_str(), Some("Track Title"));
        assert_eq!(json["artist"].as_str(), Some("Artist Name"));
        assert_eq!(json["album"].as_str(), Some("Album Name"));
        assert_eq!(json["duration_secs"].as_u64(), Some(240));
        assert_eq!(json["position_secs"].as_u64(), Some(45));

        assert_eq!(json["media_title"].as_str(), Some("Track Title"));
        assert_eq!(json["media_duration"].as_u64(), Some(240));

        let supported = json["supported_actions"]
            .as_array()
            .expect("supported_actions array");
        assert!(supported
            .iter()
            .any(|item| item.as_str() == Some("play_media")));
        assert!(supported.iter().any(|item| item.as_str() == Some("seek")));

        let enrichments = json["ui_enrichments"]
            .as_array()
            .expect("ui_enrichments array");
        assert!(enrichments
            .iter()
            .any(|item| item.as_str() == Some("favorites")));
        assert!(enrichments
            .iter()
            .any(|item| item.as_str() == Some("grouping")));

        assert_eq!(json["sonos"]["favorites"][0].as_str(), Some("Jazz"));
        assert_eq!(
            json["sonos"]["group_coordinator"].as_str(),
            Some("media.living_room")
        );
    }
}
