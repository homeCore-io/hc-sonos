mod api;
mod bridge;
mod config;
mod discovery;
mod events;
mod logging;
mod shared_state;
mod speaker;
mod subscription;

use anyhow::Result;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

use config::SonosConfig;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match SonosConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-sonos plugin");
        match try_start(&cfg, &config_path, log_level_handle.clone(), mqtt_log_handle.clone()).await {
            Ok(()) => return,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    error!(error = %e, attempt, "Startup failed; retrying in {RETRY_DELAY_SECS} s");
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                } else {
                    error!(error = %e, "Startup failed after {MAX_ATTEMPTS} attempts; exiting");
                    std::process::exit(1);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn init_logging(config_path: &str) -> (tracing_appender::non_blocking::WorkerGuard, hc_logging::LogLevelHandle, plugin_sdk_rs::mqtt_log_layer::MqttLogHandle) {
    #[derive(serde::Deserialize, Default)]
    struct Bootstrap {
        #[serde(default)]
        logging: logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    logging::init_logging(config_path, "hc-sonos", "hc_sonos=info", &bootstrap.logging)
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

async fn try_start(cfg: &SonosConfig, config_path: &str, log_level_handle: hc_logging::LogLevelHandle, mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle) -> Result<()> {
    // ── Shared Sonos speaker state (bridge + HTTP API) ─────────────────────
    let app_state = shared_state::new_state();

    // ── HomeCore SDK ─────────────────────────────────────────────────────
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id:   cfg.homecore.plugin_id.clone(),
        password:    cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config).await?;
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    let (cmd_tx, cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    // Enable management protocol (heartbeat + remote config/log commands).
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?;

    // ── Discovery channel ─────────────────────────────────────────────────
    let (discovery_tx, discovery_rx) = mpsc::channel::<sonor::Speaker>(32);

    // ── Spawn SDK event loop ─────────────────────────────────────────────
    let cmd_tx_clone = cmd_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                move |device_id, payload| {
                    let _ = cmd_tx_clone.try_send((device_id, payload));
                },
                mgmt,
            )
            .await
        {
            error!(error = %e, "SDK event loop exited with error");
        }
    });

    // ── Spawn discovery task ──────────────────────────────────────────────
    discovery::spawn(
        Duration::from_secs(cfg.sonos.discovery_interval_secs),
        Duration::from_secs(cfg.sonos.discovery_timeout_secs),
        cfg.sonos.manual_hosts.clone(),
        discovery_tx,
    );

    // ── GENA event channel (Sonos → API handler → bridge) ────────────────
    let (event_tx, event_rx) = mpsc::channel::<(String, events::NotifyEvent)>(256);

    // ── Spawn HTTP API server ──────────────────────────────────────────────
    if cfg.api.enabled {
        let api_state = app_state.clone();
        let api_host = cfg.api.host.clone();
        let api_port = cfg.api.port;
        let api_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = api::serve(&api_host, api_port, api_state, api_tx).await {
                error!(error = %e, "HTTP API server failed");
            }
        });
    }

    info!(
        discovery_interval_secs = cfg.sonos.discovery_interval_secs,
        manual_hosts = cfg.sonos.manual_hosts.len(),
        api_enabled = cfg.api.enabled,
        api_port = cfg.api.port,
        callback_host = cfg.api.callback_host.as_deref().unwrap_or("127.0.0.1"),
        "hc-sonos started (GENA mode)"
    );

    // ── Run bridge (blocks until command channel closes) ─────────────────
    let bridge = bridge::Bridge::new(cfg, publisher, app_state);
    bridge.run(discovery_rx, cmd_rx, event_rx).await;

    Ok(())
}
