//! Configuration types loaded from `config/config.toml`.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Operator-config JSON Schema, published on the capability manifest so the
/// hc-web editor renders a typed form. `None` without the `schema` feature.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(SonosConfig)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// The plugin's own **config descriptor** — how this configuration should be
/// presented, which a JSON Schema cannot express: units, conditionals, live
/// data sources, and prose.
///
/// Published on the capability manifest; core serves it at
/// `GET /plugins/{id}/config/descriptor` and the editor renders it directly
/// instead of guessing a form from the schema.
///
/// Note the Speakers section binds to the **live device registry** rather than
/// this file's `[[devices]]` array: naming and room assignment belong to the
/// device registry (core owns inventory), so those edits go to `/devices`.
pub fn config_descriptor() -> serde_json::Value {
    serde_json::json!({
        "plugin_id": "plugin.sonos",
        "descriptor_version": 1,
        "title": "Sonos",
        "sections": [
            {
                "id": "discovery",
                "title": "Discovery",
                "fields": [
                    {
                        "key": "sonos.discovery_interval_secs",
                        "kind": "duration", "unit": "secs",
                        "label": "Discovery interval", "default": 60, "min": 5,
                        "help": "How often to re-run SSDP discovery."
                    },
                    {
                        "key": "sonos.discovery_timeout_secs",
                        "kind": "duration", "unit": "secs",
                        "label": "Scan duration", "default": 5, "min": 1,
                        "help": "How long each SSDP scan listens."
                    },
                    {
                        "key": "sonos.manual_hosts",
                        "kind": "list", "item": "host",
                        "label": "Manual hosts", "default": [],
                        "help": "Static speaker IPs to probe in addition to SSDP \
                                 — useful across subnets where multicast is dropped."
                    }
                ]
            },
            {
                "id": "api",
                "title": "HTTP API",
                "fields": [
                    { "key": "api.enabled", "kind": "toggle",
                      "label": "Enable HTTP API", "default": true },
                    { "kind": "note",
                      "text": "A standalone web interface (independent of homeCore) for \
                               exploring each speaker — browse favorites and playlists, see \
                               now-playing and group state, and read diagnostics. Handy for \
                               content discovery and debugging.",
                      "visible_when": { "field": "api.enabled", "truthy": true } },
                    { "kind": "link", "label": "Open web interface",
                      "help": "Opens the Sonos HTTP API in a new tab.",
                      "href": "http://{client_host}:{api.port}/",
                      "visible_when": { "field": "api.enabled", "truthy": true } },
                    { "key": "api.host", "kind": "host", "label": "Bind address",
                      "default": "0.0.0.0",
                      "visible_when": { "field": "api.enabled", "truthy": true } },
                    { "key": "api.port", "kind": "port", "label": "Port",
                      "default": 5005,
                      "visible_when": { "field": "api.enabled", "truthy": true } },
                    { "key": "api.callback_host", "kind": "host",
                      "label": "Callback host",
                      "help": "The LAN IP speakers reach for GENA event callbacks.",
                      "visible_when": { "field": "api.enabled", "truthy": true },
                      "required_when": { "field": "api.host", "in": ["0.0.0.0", "::"] } },
                    { "kind": "note",
                      "text": "When the API binds all interfaces (0.0.0.0), speakers need a \
                               concrete LAN IP to deliver event callbacks — set Callback host \
                               to this machine's address.",
                      "visible_when": { "field": "api.host", "in": ["0.0.0.0", "::"] } }
                ]
            },
            {
                "id": "speakers",
                "title": "Speakers",
                "fields": [
                    {
                        "key": "devices",
                        "kind": "table", "render": "cards", "key_by": "device_id",
                        "label": "Speakers",
                        "help": "Every discovered speaker — set its name and room.",
                        "source": {
                            "kind": "core_resource", "ref": "devices",
                            "item_key": "device_id",
                            "labels": { "title": "name", "subtitle": "device_id" }
                        },
                        "item": [
                            { "key": "name", "kind": "text", "label": "Name" },
                            { "key": "area", "kind": "select", "label": "Room",
                              "placeholder": "Unassigned", "allow_create": true,
                              "source": { "kind": "core_resource", "ref": "areas" } }
                        ]
                    }
                ]
            },
            {
                "id": "logging",
                "title": "Logging",
                "fields": [
                    { "key": "logging.level", "kind": "text", "label": "Level",
                      "default": "info",
                      "placeholder": "info | debug | hc_sonos=debug" },
                    { "key": "logging.log_forward_level", "kind": "enum",
                      "render": "segmented", "label": "Forward to core", "default": "info",
                      "options": [
                          {"value": "off", "label": "Off"},
                          {"value": "error", "label": "Error"},
                          {"value": "warn", "label": "Warn"},
                          {"value": "info", "label": "Info"},
                          {"value": "debug", "label": "Debug"}
                      ] }
                ]
            },
            {
                "id": "connection",
                "title": "Connection",
                "hidden": true,
                "fields": [
                    { "key": "homecore.broker_host", "kind": "host", "label": "Broker host" },
                    { "key": "homecore.broker_port", "kind": "port", "label": "Broker port" },
                    { "key": "homecore.password", "kind": "secret", "label": "Broker password" }
                ]
            }
        ]
    })
}

#[derive(Deserialize, Clone, Debug, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SonosConfig {
    #[serde(default)]
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub sonos: SonosSection,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub logging: crate::logging::LoggingConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

impl SonosConfig {
    pub fn load(path: &str) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config from {path}"))?;
        toml::from_str(&text).context("parsing config TOML")
    }
}

#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HomecoreConfig {
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

impl Default for HomecoreConfig {
    fn default() -> Self {
        Self {
            broker_host: default_broker_host(),
            broker_port: default_broker_port(),
            plugin_id: default_plugin_id(),
            password: String::new(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SonosSection {
    /// How often to re-run SSDP discovery (seconds).
    #[serde(default = "default_discovery_interval_secs")]
    pub discovery_interval_secs: u64,
    /// SSDP scan duration (seconds).
    #[serde(default = "default_discovery_timeout_secs")]
    pub discovery_timeout_secs: u64,
    /// Static IPs to probe in addition to SSDP.
    #[serde(default)]
    pub manual_hosts: Vec<String>,
}

impl Default for SonosSection {
    fn default() -> Self {
        Self {
            discovery_interval_secs: default_discovery_interval_secs(),
            discovery_timeout_secs: default_discovery_timeout_secs(),
            manual_hosts: vec![],
        }
    }
}

/// A pre-configured speaker entry.  Any speaker discovered via SSDP that
/// has a matching UUID will use these hc_id / name / area values instead of
/// the auto-generated ones.
#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceConfig {
    /// Sonos speaker UUID (e.g. "RINCON_347E5C3D12E401400").
    pub uuid: String,
    /// HomeCore device ID (e.g. "sonos_living_room").
    pub hc_id: String,
    /// Human-readable name surfaced in HomeCore.
    pub name: String,
    /// Optional room / area assignment.
    pub area: Option<String>,
}

/// HTTP API configuration.  The API runs its own Axum server, completely
/// independent of HomeCore.  Disable with `enabled = false`.
#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiConfig {
    #[serde(default = "default_api_host")]
    pub host: String,
    #[serde(default = "default_api_port")]
    pub port: u16,
    /// Set to false to disable the HTTP API entirely.
    #[serde(default = "default_api_enabled")]
    pub enabled: bool,
    /// The IP or hostname Sonos speakers can reach to deliver GENA NOTIFY
    /// callbacks.  Required when `host` is `"0.0.0.0"` (i.e. any interface).
    /// Example: `callback_host = "192.168.1.10"`.
    /// Defaults to `"127.0.0.1"` when not set (loopback only — useful for
    /// local testing; set to your LAN IP in production).
    pub callback_host: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: default_api_host(),
            port: default_api_port(),
            enabled: default_api_enabled(),
            callback_host: None,
        }
    }
}

// ── defaults ─────────────────────────────────────────────────────────────────

fn default_api_host() -> String {
    "0.0.0.0".into()
}
fn default_api_port() -> u16 {
    5005
}
fn default_api_enabled() -> bool {
    true
}
fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
}
fn default_plugin_id() -> String {
    "plugin.sonos".into()
}
fn default_discovery_interval_secs() -> u64 {
    60
}
fn default_discovery_timeout_secs() -> u64 {
    5
}
