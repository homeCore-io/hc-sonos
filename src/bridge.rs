//! Main bridge event loop.
//!
//! Speaker handles and last-known state live in `SharedState` so the HTTP API
//! can access them directly without going through HomeCore.
//!
//! State changes are driven by UPnP GENA NOTIFY events rather than polling.
//! A lightweight heartbeat (every 60 s) detects offline/recovered speakers and
//! re-subscribes them when they come back.  Zone-group topology is polled
//! every 5 minutes because Sonos has no GENA event for it.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use sonor::Speaker;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{DeviceConfig, SonosConfig};
use crate::events::{NotifyEvent};
use crate::homecore::HomecorePublisher;
use crate::shared_state::{AppState, SpeakerEntry};
use crate::speaker::{self, SpeakerState};
use crate::subscription;

const HEARTBEAT_SECS: u64  = 60;
const ZONE_POLL_SECS: u64  = 300;

pub struct Bridge {
    state:         AppState,
    hc_to_uuid:    HashMap<String, String>,
    config_map:    HashMap<String, DeviceConfig>,
    publisher:     HomecorePublisher,
    /// Base URL for GENA callbacks, e.g. `"http://192.168.1.10:5005"`.
    callback_base: String,
}

impl Bridge {
    pub fn new(cfg: &SonosConfig, publisher: HomecorePublisher, state: AppState) -> Self {
        let config_map = cfg.devices.iter()
            .map(|d| (d.uuid.clone(), d.clone()))
            .collect();
        let callback_host = cfg.api.callback_host.clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let callback_base = format!("http://{}:{}", callback_host, cfg.api.port);
        Self {
            state,
            hc_to_uuid:   HashMap::new(),
            config_map,
            publisher,
            callback_base,
        }
    }

    pub async fn run(
        mut self,
        mut discovery_rx: mpsc::Receiver<Speaker>,
        mut homecore_rx:  mpsc::Receiver<(String, Value)>,
        mut event_rx:     mpsc::Receiver<(String, NotifyEvent)>,
    ) {
        let mut heartbeat  = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        let mut zone_timer = tokio::time::interval(Duration::from_secs(ZONE_POLL_SECS));
        // Consume the immediate tick so we don't fire zone polling at startup
        // before any speakers are known.
        heartbeat.tick().await;
        zone_timer.tick().await;

        info!("Bridge event loop running (GENA mode)");
        loop {
            tokio::select! {
                Some(speaker) = discovery_rx.recv() =>
                    self.handle_discovered(speaker).await,

                cmd = homecore_rx.recv() => match cmd {
                    Some((hc_id, payload)) => self.handle_command(hc_id, payload).await,
                    None => { info!("HomeCore channel closed"); return; }
                },

                Some((uuid, event)) = event_rx.recv() =>
                    self.handle_gena_event(uuid, event).await,

                _ = heartbeat.tick() =>
                    self.heartbeat().await,

                _ = zone_timer.tick() =>
                    self.poll_zone_groups().await,
            }
        }
    }

    // ── Discovery ─────────────────────────────────────────────────────────────

    async fn handle_discovered(&mut self, speaker: Speaker) {
        let uuid: String = match speaker.uuid().await {
            Ok(u) => u,
            Err(e) => { warn!(error = %e, "Could not get UUID; skipping"); return; }
        };

        // If already known, just refresh the handle (IP may have changed).
        {
            let mut st = self.state.write().await;
            if let Some(entry) = st.speakers.get_mut(&uuid) {
                entry.speaker = speaker.clone();
                debug!(uuid, "Refreshed speaker handle");
            }
        }

        // Check if already registered
        let already_known = self.state.read().await.speakers.contains_key(&uuid);

        if already_known {
            // Re-subscribe in case we lost the subscription (e.g. speaker rebooted).
            let host_port = speaker_host_port(&speaker);
            subscription::subscribe_speaker(host_port, uuid, self.callback_base.clone());
            return;
        }

        let room_name: String = match speaker.name().await {
            Ok(n) => n,
            Err(e) => { warn!(uuid, error = %e, "Could not get room name; skipping"); return; }
        };

        let (hc_id, display_name, area): (String, String, Option<String>) =
            if let Some(cfg) = self.config_map.get(&uuid) {
                (cfg.hc_id.clone(), cfg.name.clone(), cfg.area.clone())
            } else {
                let sanitized: String = room_name.to_lowercase()
                    .chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
                (format!("sonos_{sanitized}"), room_name.clone(), None)
            };

        info!(uuid, hc_id, room_name, "Registering new Sonos speaker");

        if let Err(e) = self.publisher
            .register_device(&hc_id, &display_name, "media_player", area.as_deref()).await
        { warn!(hc_id, error = %e, "Failed to register device"); }
        if let Err(e) = self.publisher.subscribe_commands(&hc_id).await
        { warn!(hc_id, error = %e, "Failed to subscribe to commands"); }
        if let Err(e) = self.publisher.publish_availability(&hc_id, true).await
        { warn!(hc_id, error = %e, "Failed to publish availability"); }

        // Initial state poll so HomeCore has state before the first GENA event.
        let initial_state = match speaker::poll(&speaker).await {
            Ok(s) => {
                debug!(hc_id, "Initial poll succeeded");
                Some(s)
            }
            Err(e) => {
                warn!(hc_id, error = %e, "Initial poll failed");
                None
            }
        };

        {
            let mut st = self.state.write().await;
            st.uuid_to_room.insert(uuid.clone(), room_name.clone());
            st.room_to_uuid.insert(room_name.to_lowercase(), uuid.clone());
            st.speakers.insert(uuid.clone(), SpeakerEntry {
                speaker: speaker.clone(),
                uuid:      uuid.clone(),
                hc_id:     hc_id.clone(),
                room_name,
                available: true,
                last_state: initial_state.clone(),
            });
        }
        self.hc_to_uuid.insert(hc_id.clone(), uuid.clone());

        if let Some(state) = initial_state {
            let json    = speaker::to_json(&state);
            let pub2    = self.publisher.clone();
            let hc_id2  = hc_id.clone();
            tokio::spawn(async move {
                if let Err(e) = pub2.publish_state(&hc_id2, &json).await {
                    warn!(hc_id = hc_id2, error = %e, "Failed to publish initial state");
                }
            });
        }

        // Subscribe to GENA events from this speaker.
        let host_port = speaker_host_port(&speaker);
        subscription::subscribe_speaker(host_port, uuid, self.callback_base.clone());
    }

    // ── GENA event handling ───────────────────────────────────────────────────

    async fn handle_gena_event(&mut self, uuid: String, event: NotifyEvent) {
        let (hc_id, state_to_publish) = {
            let mut st = self.state.write().await;
            let entry = match st.speakers.get_mut(&uuid) {
                Some(e) => e,
                None => {
                    debug!(uuid, "GENA event for unknown speaker — ignored");
                    return;
                }
            };

            let hc_id = entry.hc_id.clone();

            // Clone or default the current state, apply the partial update.
            let mut new_state = entry.last_state.clone().unwrap_or_default();
            match &event {
                NotifyEvent::Avt(avt) => new_state.apply_avt(avt),
                NotifyEvent::Rc(rc)   => new_state.apply_rc(rc),
            }

            let changed = entry.last_state.as_ref() != Some(&new_state);
            if changed {
                entry.last_state = Some(new_state.clone());
                (hc_id, Some(new_state))
            } else {
                (hc_id, None)
            }
        };

        if let Some(state) = state_to_publish {
            let json = speaker::to_json(&state);
            let pub2 = self.publisher.clone();
            tokio::spawn(async move {
                if let Err(e) = pub2.publish_state(&hc_id, &json).await {
                    warn!(hc_id, error = %e, "Failed to publish GENA state");
                } else {
                    debug!(hc_id, "State published via GENA");
                }
            });
        }
    }

    // ── Heartbeat ─────────────────────────────────────────────────────────────

    /// Ping every known speaker.  Mark offline on failure; re-subscribe and
    /// publish availability on recovery.
    async fn heartbeat(&mut self) {
        let handles: Vec<(String, Speaker, bool)> = {
            let st = self.state.read().await;
            st.speakers.values()
                .map(|e| (e.uuid.clone(), e.speaker.clone(), e.available))
                .collect()
        };

        for (uuid, speaker, was_available) in handles {
            let reachable = speaker.is_playing().await.is_ok();

            match (was_available, reachable) {
                (true, false) => {
                    // Speaker went offline
                    let hc_id = {
                        let mut st = self.state.write().await;
                        st.speakers.get_mut(&uuid).map(|e| {
                            e.available = false;
                            e.hc_id.clone()
                        })
                    };
                    if let Some(hc_id) = hc_id {
                        warn!(hc_id, "Speaker unreachable — marking offline");
                        let pub2 = self.publisher.clone();
                        tokio::spawn(async move {
                            let _ = pub2.publish_availability(&hc_id, false).await;
                        });
                    }
                }
                (false, true) => {
                    // Speaker recovered
                    let hc_id = {
                        let mut st = self.state.write().await;
                        st.speakers.get_mut(&uuid).map(|e| {
                            e.available = true;
                            e.hc_id.clone()
                        })
                    };
                    if let Some(hc_id) = hc_id {
                        info!(hc_id, "Speaker recovered — re-subscribing");
                        let pub2 = self.publisher.clone();
                        let hc2  = hc_id.clone();
                        tokio::spawn(async move {
                            let _ = pub2.publish_availability(&hc2, true).await;
                        });
                        // Re-subscribe to restore GENA delivery.
                        let host_port = speaker_host_port(&speaker);
                        subscription::subscribe_speaker(
                            host_port,
                            uuid.clone(),
                            self.callback_base.clone(),
                        );
                        // Fresh poll to get current state immediately.
                        if let Ok(state) = speaker::poll(&speaker).await {
                            let mut st = self.state.write().await;
                            if let Some(entry) = st.speakers.get_mut(&uuid) {
                                entry.last_state = Some(state.clone());
                            }
                            let json = speaker::to_json(&state);
                            let pub2 = self.publisher.clone();
                            tokio::spawn(async move {
                                let _ = pub2.publish_state(&hc_id, &json).await;
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // ── Zone group topology ───────────────────────────────────────────────────

    async fn poll_zone_groups(&mut self) {
        let handles: Vec<(String, Speaker)> = {
            let st = self.state.read().await;
            if st.speakers.is_empty() { return; }
            st.speakers.values().map(|e| (e.uuid.clone(), e.speaker.clone())).collect()
        };

        let zone_groups = self.fetch_zone_groups(&handles).await;
        if zone_groups.is_empty() { return; }

        // uuid → (coord_uuid, member_uuids)
        let mut group_by_uuid: HashMap<String, (String, Vec<String>)> = HashMap::new();
        for (coord_uuid, members) in &zone_groups {
            let member_uuids: Vec<String> =
                members.iter().map(|m| m.uuid().to_string()).collect();
            for member in members {
                group_by_uuid.insert(
                    member.uuid().to_string(),
                    (coord_uuid.clone(), member_uuids.clone()),
                );
            }
        }

        // Snapshot uuid → hc_id mapping before taking write lock.
        let uuid_to_hc: HashMap<String, String> = {
            let st = self.state.read().await;
            st.speakers.iter().map(|(u, e)| (u.clone(), e.hc_id.clone())).collect()
        };

        let mut to_publish: Vec<(String, serde_json::Value)> = Vec::new();
        {
            let mut st = self.state.write().await;
            for (uuid, (coord_uuid, member_uuids)) in &group_by_uuid {
                let coord_hc  = uuid_to_hc.get(coord_uuid).cloned();
                let member_hc: Vec<String> = member_uuids.iter()
                    .filter_map(|u| uuid_to_hc.get(u).cloned()).collect();

                if let Some(entry) = st.speakers.get_mut(uuid) {
                    let state = entry.last_state.get_or_insert_with(SpeakerState::default);
                    if state.group_coordinator != coord_hc || state.group_members != member_hc {
                        state.group_coordinator = coord_hc;
                        state.group_members     = member_hc;
                        to_publish.push((entry.hc_id.clone(), speaker::to_json(state)));
                    }
                }
            }
        }

        for (hc_id, json) in to_publish {
            let pub2 = self.publisher.clone();
            tokio::spawn(async move {
                if let Err(e) = pub2.publish_state(&hc_id, &json).await {
                    warn!(hc_id, error = %e, "Failed to publish zone topology");
                }
            });
        }
    }

    async fn fetch_zone_groups(
        &self,
        handles: &[(String, Speaker)],
    ) -> HashMap<String, Vec<sonor::SpeakerInfo>> {
        for (uuid, speaker) in handles {
            let avail = {
                let st = self.state.read().await;
                st.speakers.get(uuid).map(|e| e.available).unwrap_or(false)
            };
            if avail {
                match speaker.zone_group_state().await {
                    Ok(g) => return g,
                    Err(e) => warn!(error = %e, "zone_group_state failed"),
                }
            }
        }
        HashMap::new()
    }

    // ── HomeCore command handling ─────────────────────────────────────────────

    async fn handle_command(&mut self, hc_id: String, cmd: Value) {
        let uuid = match self.hc_to_uuid.get(&hc_id) {
            Some(u) => u.clone(),
            None => { warn!(hc_id, "Received command for unknown device"); return; }
        };
        let (speaker, available, uuid_to_room) = {
            let st = self.state.read().await;
            let entry = match st.speakers.get(&uuid) { Some(e) => e, None => return };
            (entry.speaker.clone(), entry.available, st.uuid_to_room.clone())
        };
        if !available { warn!(hc_id, "Ignoring command — speaker is offline"); return; }
        if let Err(e) = speaker::execute_command(&speaker, &cmd, &uuid_to_room).await {
            warn!(hc_id, error = %e, ?cmd, "Command failed");
        } else {
            debug!(hc_id, action = ?cmd["action"], "Command executed");
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract `"host:1400"` from a Speaker's device URL.
fn speaker_host_port(speaker: &Speaker) -> String {
    let url  = speaker.device().url();
    let host = url.host().unwrap_or("127.0.0.1");
    let port = url.port_u16().unwrap_or(1400);
    format!("{host}:{port}")
}
