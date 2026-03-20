//! Shared speaker state — owned jointly by the bridge and the HTTP API.
//!
//! The bridge writes speaker handles and polled state here.
//! The HTTP API reads speaker handles to send commands and reads last-known
//! state for informational endpoints.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use sonor::Speaker;

use crate::speaker::SpeakerState;

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

pub struct SpeakerEntry {
    pub speaker:    Speaker,
    pub uuid:       String,
    pub hc_id:      String,
    pub room_name:  String,
    pub available:  bool,
    pub last_state: Option<SpeakerState>,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct SharedState {
    /// uuid → entry
    pub speakers:     HashMap<String, SpeakerEntry>,
    /// lowercase room name → uuid  (HTTP routing)
    pub room_to_uuid: HashMap<String, String>,
    /// uuid → original room name
    pub uuid_to_room: HashMap<String, String>,
}

impl SharedState {
    /// Find a speaker by room name (case-insensitive).
    pub fn find_by_room(&self, room: &str) -> Option<&SpeakerEntry> {
        let lower = room.to_lowercase();
        let uuid = self.room_to_uuid.get(&lower)?;
        self.speakers.get(uuid)
    }

    /// Return all UUIDs.
    pub fn uuids(&self) -> Vec<String> {
        self.speakers.keys().cloned().collect()
    }
}

pub type AppState = Arc<RwLock<SharedState>>;

pub fn new_state() -> AppState {
    Arc::new(RwLock::new(SharedState::default()))
}
