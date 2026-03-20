//! Main bridge event loop.
//!
//! Speaker handles and last-known state live in `SharedState` so the HTTP API
//! can access them directly without going through HomeCore.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use sonor::Speaker;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{DeviceConfig, SonosConfig};
use crate::homecore::HomecorePublisher;
use crate::shared_state::{AppState, SpeakerEntry};
use crate::speaker::{self, SpeakerState};

pub struct Bridge {
    state:        AppState,
    hc_to_uuid:   HashMap<String, String>,
    config_map:   HashMap<String, DeviceConfig>,
    publisher:    HomecorePublisher,
    poll_interval: Duration,
}

impl Bridge {
    pub fn new(cfg: &SonosConfig, publisher: HomecorePublisher, state: AppState) -> Self {
        let config_map = cfg.devices.iter()
            .map(|d| (d.uuid.clone(), d.clone()))
            .collect();
        Self {
            state,
            hc_to_uuid:   HashMap::new(),
            config_map,
            publisher,
            poll_interval: Duration::from_secs(cfg.sonos.poll_interval_secs),
        }
    }

    pub async fn run(
        mut self,
        mut discovery_rx: mpsc::Receiver<Speaker>,
        mut homecore_rx:  mpsc::Receiver<(String, Value)>,
    ) {
        let mut poll_timer = tokio::time::interval(self.poll_interval);
        poll_timer.tick().await;
        info!("Bridge event loop running");
        loop {
            tokio::select! {
                Some(speaker) = discovery_rx.recv() => self.handle_discovered(speaker).await,
                cmd = homecore_rx.recv() => match cmd {
                    Some((hc_id, payload)) => self.handle_command(hc_id, payload).await,
                    None => { info!("HomeCore channel closed"); return; }
                },
                _ = poll_timer.tick() => self.poll_all().await,
            }
        }
    }

    async fn handle_discovered(&mut self, speaker: Speaker) {
        let uuid: String = match speaker.uuid().await {
            Ok(u) => u,
            Err(e) => { warn!(error = %e, "Could not get UUID; skipping"); return; }
        };

        {
            let mut st = self.state.write().await;
            if let Some(entry) = st.speakers.get_mut(&uuid) {
                entry.speaker = speaker;
                debug!(uuid, "Updated speaker handle");
                return;
            }
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

        {
            let mut st = self.state.write().await;
            st.uuid_to_room.insert(uuid.clone(), room_name.clone());
            st.room_to_uuid.insert(room_name.to_lowercase(), uuid.clone());
            st.speakers.insert(uuid.clone(), SpeakerEntry {
                speaker, uuid: uuid.clone(), hc_id: hc_id.clone(),
                room_name, available: true, last_state: None,
            });
        }
        self.hc_to_uuid.insert(hc_id, uuid);
    }

    async fn poll_all(&mut self) {
        let handles: Vec<(String, Speaker)> = {
            let st = self.state.read().await;
            if st.speakers.is_empty() { return; }
            st.speakers.values().map(|e| (e.uuid.clone(), e.speaker.clone())).collect()
        };

        let zone_groups = self.fetch_zone_groups(&handles).await;

        let mut group_info: HashMap<String, (String, Vec<String>)> = HashMap::new();
        for (coord_uuid, members) in &zone_groups {
            let member_uuids: Vec<String> = members.iter().map(|m| m.uuid().to_string()).collect();
            for member in members {
                group_info.insert(member.uuid().to_string(), (coord_uuid.clone(), member_uuids.clone()));
            }
        }

        let mut results: Vec<(String, anyhow::Result<SpeakerState>)> = Vec::new();
        for (uuid, sp) in &handles {
            results.push((uuid.clone(), speaker::poll(sp).await.map_err(anyhow::Error::from)));
        }

        for (uuid, poll_result) in results {
            match poll_result {
                Ok(mut new_state) => {
                    if let Some((coord_uuid, member_uuids)) = group_info.get(&uuid) {
                        let st = self.state.read().await;
                        new_state.group_coordinator = st.speakers.get(coord_uuid).map(|e| e.hc_id.clone());
                        new_state.group_members = member_uuids.iter()
                            .filter_map(|u| st.speakers.get(u).map(|e| e.hc_id.clone())).collect();
                    }
                    let mut st = self.state.write().await;
                    if let Some(entry) = st.speakers.get_mut(&uuid) {
                        if !entry.available {
                            entry.available = true;
                            let hc_id = entry.hc_id.clone(); let pub2 = self.publisher.clone();
                            tokio::spawn(async move { let _ = pub2.publish_availability(&hc_id, true).await; });
                            info!(hc_id = entry.hc_id, "Speaker came back online");
                        }
                        if entry.last_state.as_ref() != Some(&new_state) {
                            let json = speaker::to_json(&new_state);
                            entry.last_state = Some(new_state);
                            let hc_id = entry.hc_id.clone(); let pub2 = self.publisher.clone();
                            tokio::spawn(async move {
                                if let Err(e) = pub2.publish_state(&hc_id, &json).await {
                                    warn!(hc_id, error = %e, "Failed to publish state");
                                } else { debug!(hc_id, "State published"); }
                            });
                        }
                    }
                }
                Err(e) => {
                    let mut st = self.state.write().await;
                    if let Some(entry) = st.speakers.get_mut(&uuid) {
                        if entry.available {
                            entry.available = false;
                            warn!(hc_id = entry.hc_id, error = %e, "Speaker unreachable — marking offline");
                            let hc_id = entry.hc_id.clone(); let pub2 = self.publisher.clone();
                            tokio::spawn(async move { let _ = pub2.publish_availability(&hc_id, false).await; });
                        }
                    }
                }
            }
        }
    }

    async fn fetch_zone_groups(&self, handles: &[(String, Speaker)]) -> HashMap<String, Vec<sonor::SpeakerInfo>> {
        for (uuid, speaker) in handles {
            let avail = { let st = self.state.read().await; st.speakers.get(uuid).map(|e| e.available).unwrap_or(false) };
            if avail {
                match speaker.zone_group_state().await {
                    Ok(g) => return g,
                    Err(e) => warn!(error = %e, "zone_group_state failed"),
                }
            }
        }
        HashMap::new()
    }

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
