//! S-101 Explorer plugin.
//!
//! In-process index browser. Reads chart data through the HostApi's
//! `chart_*` methods (added in PLUGIN_API_VERSION 2) — no external
//! processes, no re-parsing of the chart file. The host already has
//! the cell + Feature Catalogue + indices; this plugin is a thin UI
//! over them.
//!
//! UI events fire from the host (egui side panel) and post into
//! `handle_ui_event`. The plugin runs the matching `HostApi::chart_*`
//! call and stores the JSON result; the host fetches it via
//! `get_ui_data` for the next redraw cycle.

use std::sync::OnceLock;

use abi_stable::{
    sabi_trait::TD_Opaque,
    std_types::{RBox, ROption, RStr, RString, RVec},
};
use serde::{Deserialize, Serialize};

use ferrite_plugin_api::{
    DrawingInstruction, HostApi, MouseEvent, Plugin, PluginMetadata, PluginModule, Plugin_TO,
    Position, SettingsItem, PLUGIN_API_VERSION,
};

const PLUGIN_ID: &str = "com.ferrite.s101-explorer";
const PLUGIN_NAME: &str = "S-101 Explorer";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Default)]
pub struct ExplorerPlugin {
    panel_visible: bool,
    host_api: Option<HostApi>,
    /// Most recent query result rendered in the panel's "Result" area.
    last_result: Option<String>,
    /// Last operation summary (shown above the result).
    last_op: Option<String>,
    settings: Settings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Cap on result list lengths returned by the panel; the HostApi already
    /// caps internally too, but keeping this user-visible makes debug runs
    /// less noisy.
    pub max_results: u32,
}

impl ExplorerPlugin {
    pub fn new() -> Self {
        Self {
            settings: Settings { max_results: 50 },
            ..Self::default()
        }
    }

    fn refresh_ui(&self) {
        if let Some(api) = &self.host_api {
            api.refresh_ui();
        }
    }

    fn run_query<F: FnOnce(&HostApi) -> String>(&mut self, label: &str, f: F) {
        let Some(api) = self.host_api.clone() else {
            self.last_result = Some(r#"{"error":"no host api"}"#.to_string());
            return;
        };
        if !api.chart_loaded() {
            self.last_result =
                Some(r#"{"error":"no chart loaded — open one via File > Open"}"#.to_string());
            self.last_op = Some(label.to_string());
            return;
        }
        let raw = f(&api);
        // Pretty-print for the side-panel monospace box. If parsing fails,
        // surface the raw response so a host-side error is still visible.
        let pretty = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or(raw);
        self.last_result = Some(pretty);
        self.last_op = Some(label.to_string());
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum UiEvent {
    CatalogueSearch {
        term: String,
    },
    CatalogueDescribeFeature {
        code: String,
    },
    CatalogueDescribeAttribute {
        code: String,
    },
    FeatureGet {
        id: i64,
    },
    FeatureQueryBbox {
        w: f64,
        s: f64,
        e: f64,
        n: f64,
    },
    FeatureNearby {
        lat: f64,
        lon: f64,
        radius_m: f64,
    },
    DatasetMetadata,
}

#[derive(Debug, Serialize)]
struct PanelData {
    chart_loaded: bool,
    metadata_summary: String,
    last_op: Option<String>,
    last_result: Option<String>,
    /// Echoes back the last search term so the host's text field can
    /// pre-fill from server state if the user reopens the panel.
    input_search: String,
    /// Cap visible to the user.
    max_results: u32,
    /// HTTP MCP server registration info. Always present when the host
    /// has the server enabled — the chart-loaded gate is at the tool
    /// level, not the registration level, so users can register Claude
    /// before opening a chart and tools start responding the moment
    /// they do.
    connection_info: Option<ConnectionInfo>,
}

#[derive(Debug, Serialize)]
struct ConnectionInfo {
    /// Server lifecycle state from the host.
    running: bool,
    /// `http://127.0.0.1:PORT/mcp` for clients on the same machine.
    local_url: String,
    /// `https://*.ngrok-free.app/mcp` (or paid-plan domain) once the
    /// ngrok tunnel is up.
    public_url: Option<String>,
    /// "ready" | "starting" | "unavailable" | "disabled"
    tunnel_state: String,
    /// User-facing message — install/authtoken hint when ngrok is
    /// missing, timeout note when the tunnel didn't come up, etc.
    tunnel_message: String,
    /// Auth scheme advertised by the server; always "oauth2" now.
    auth: String,
    /// Number of OAuth Dynamic Client Registrations performed so far.
    /// Surfaces in the UI as "1 client registered" / etc.
    registered_clients: u64,
    /// Discovery URL clients hit to learn how to authenticate. Used in
    /// the panel's troubleshooting section (paste in a browser → JSON).
    discovery_url: String,
    /// `claude_desktop_config.json` snippet for the HTTP transport with
    /// OAuth. Modern MCP clients auto-discover OAuth via the 401 on
    /// `/mcp`; the snippet has no `Authorization` header — the client
    /// fetches one itself.
    config_snippet: String,
    /// Raw cURL command demonstrating the 401-with-resource_metadata
    /// response. Useful for verifying the discovery flow without
    /// running a full OAuth client.
    curl_example: String,
}

/// Build the HTTP-based `ConnectionInfo` from the JSON returned by
/// `HostApi::mcp_server_info()`. Returns `None` when the server hasn't
/// reported `running: true` yet — the panel renders a "starting…"
/// placeholder in that case.
fn build_connection_info(server_info: &serde_json::Value) -> Option<ConnectionInfo> {
    let running = server_info.get("running").and_then(|v| v.as_bool())?;
    let local_url = server_info
        .get("local_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if local_url.is_empty() {
        return None;
    }
    let public_url = server_info
        .get("public_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let tunnel_state = server_info
        .get("tunnel_state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let tunnel_message = server_info
        .get("tunnel_message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let auth = server_info
        .get("auth")
        .and_then(|v| v.as_str())
        .unwrap_or("oauth2")
        .to_string();
    let registered_clients = server_info
        .get("registered_clients")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Prefer the public URL for the snippet — that's the URL Claude
    // running elsewhere actually needs. Fall back to local when the
    // tunnel isn't up yet (still works for same-machine clients).
    let registration_url = public_url.clone().unwrap_or_else(|| local_url.clone());

    // Strip trailing /mcp to derive the AS base — discovery endpoints
    // sit at the root of the issuer, not under /mcp.
    let base_url = registration_url
        .strip_suffix("/mcp")
        .unwrap_or(&registration_url)
        .to_string();
    let discovery_url = format!("{}/.well-known/oauth-authorization-server", base_url);

    // claude_desktop_config.json shape for the HTTP MCP transport with
    // OAuth: no headers — the client auto-discovers OAuth from the 401
    // response and runs the auth-code flow itself.
    let config = serde_json::json!({
        "mcpServers": {
            "s101": {
                "transport": "http",
                "url": registration_url
            }
        }
    });
    let config_snippet = serde_json::to_string_pretty(&config).unwrap_or_default();

    let curl_example = format!(
        "# 1. Discovery probe — should return 401 with WWW-Authenticate \
         pointing to the protected-resource metadata.\n\
         curl -i -X POST {url} \\\n  \
         -H \"Content-Type: application/json\" \\\n  \
         -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}}'\n\n\
         # 2. Read AS metadata (open in browser too):\n\
         curl {disco}",
        url = registration_url,
        disco = discovery_url
    );

    Some(ConnectionInfo {
        running,
        local_url,
        public_url,
        tunnel_state,
        tunnel_message,
        auth,
        registered_clients,
        discovery_url,
        config_snippet,
        curl_example,
    })
}

impl Plugin for ExplorerPlugin {
    fn id(&self) -> RStr<'_> {
        RStr::from(PLUGIN_ID)
    }
    fn name(&self) -> RStr<'_> {
        RStr::from(PLUGIN_NAME)
    }
    fn version(&self) -> RStr<'_> {
        RStr::from(PLUGIN_VERSION)
    }
    fn toolbar_label(&self) -> ROption<RStr<'_>> {
        ROption::RSome(RStr::from("Explorer"))
    }
    fn toolbar_tooltip(&self) -> ROption<RStr<'_>> {
        ROption::RSome(RStr::from(
            "S-101 Explorer — catalogue search, feature lookup, bbox / nearby queries",
        ))
    }
    fn is_active(&self) -> bool {
        self.panel_visible
    }
    fn set_active(&mut self, active: bool) {
        self.panel_visible = active;
        self.refresh_ui();
    }

    fn on_mouse_click(&mut self, _event: MouseEvent) -> bool {
        false
    }
    fn on_mouse_move(&mut self, _position: Position) {}
    fn get_drawing_instructions(&self) -> RVec<DrawingInstruction> {
        RVec::new()
    }

    fn get_ui_data(&self) -> RVec<u8> {
        let chart_loaded = self
            .host_api
            .as_ref()
            .map(|h| h.chart_loaded())
            .unwrap_or(false);

        let metadata: Option<serde_json::Value> = if chart_loaded {
            self.host_api
                .as_ref()
                .and_then(|h| serde_json::from_str(&h.chart_dataset_metadata()).ok())
        } else {
            None
        };

        let metadata_summary = metadata
            .as_ref()
            .and_then(|v| {
                let count = v.get("feature_count")?.as_u64()?;
                let extent = v.get("extent")?;
                let w = extent.get("w")?.as_f64()?;
                let s = extent.get("s")?.as_f64()?;
                let e = extent.get("e")?.as_f64()?;
                let n = extent.get("n")?.as_f64()?;
                Some(format!(
                    "{} features · extent [lon {:.4}…{:.4}, lat {:.4}…{:.4}]",
                    count, w, e, s, n
                ))
            })
            .unwrap_or_else(|| {
                if chart_loaded {
                    "indices ready".to_string()
                } else {
                    String::new()
                }
            });

        // The HTTP MCP server runs from app startup; its registration
        // info is independent of whether a chart is loaded.
        let server_info: Option<serde_json::Value> = self
            .host_api
            .as_ref()
            .and_then(|h| {
                let raw = h.mcp_server_info();
                if raw.is_empty() {
                    None
                } else {
                    serde_json::from_str(&raw).ok()
                }
            });
        let connection_info = server_info.as_ref().and_then(build_connection_info);

        let panel = PanelData {
            chart_loaded,
            metadata_summary,
            last_op: self.last_op.clone(),
            last_result: self.last_result.clone(),
            input_search: String::new(),
            max_results: self.settings.max_results,
            connection_info,
        };
        match serde_json::to_vec(&panel) {
            Ok(b) => RVec::from(b),
            Err(_) => RVec::new(),
        }
    }

    fn handle_ui_event(&mut self, event_json: RStr<'_>) {
        let Ok(event) = serde_json::from_str::<UiEvent>(event_json.as_str()) else {
            return;
        };
        let limit = self.settings.max_results;
        match event {
            UiEvent::CatalogueSearch { term } => {
                let label = format!("catalogue_search('{}')", term);
                self.run_query(&label, |api| api.chart_catalogue_search(&term, limit));
            }
            UiEvent::CatalogueDescribeFeature { code } => {
                let label = format!("catalogue_describe_feature('{}')", code);
                self.run_query(&label, |api| api.chart_catalogue_describe_feature(&code));
            }
            UiEvent::CatalogueDescribeAttribute { code } => {
                let label = format!("catalogue_describe_attribute('{}')", code);
                self.run_query(&label, |api| api.chart_catalogue_describe_attribute(&code));
            }
            UiEvent::FeatureGet { id } => {
                let label = format!("feature_get({})", id);
                self.run_query(&label, |api| api.chart_feature_get(id));
            }
            UiEvent::FeatureQueryBbox { w, s, e, n } => {
                let label = format!(
                    "feature_query_bbox(w={:.4}, s={:.4}, e={:.4}, n={:.4})",
                    w, s, e, n
                );
                self.run_query(&label, |api| api.chart_feature_query_bbox(w, s, e, n, limit));
            }
            UiEvent::FeatureNearby {
                lat,
                lon,
                radius_m,
            } => {
                let label = format!(
                    "feature_nearby(lat={:.4}, lon={:.4}, r={:.0}m)",
                    lat, lon, radius_m
                );
                self.run_query(&label, |api| {
                    api.chart_feature_nearby(lat, lon, radius_m, limit)
                });
            }
            UiEvent::DatasetMetadata => {
                self.run_query("dataset_metadata()", |api| api.chart_dataset_metadata());
            }
        }
        self.refresh_ui();
    }

    fn get_settings_schema(&self) -> RVec<SettingsItem> {
        let mut v = RVec::new();
        v.push(SettingsItem::Number {
            key: RString::from("max_results"),
            label: RString::from("Max results per query"),
            description: RString::from(
                "Cap on the number of features returned by bbox / nearby queries.",
            ),
            min: 1.0,
            max: 1000.0,
            step: 10.0,
            default_value: 50.0,
        });
        v
    }

    fn get_settings(&self) -> RVec<u8> {
        match serde_json::to_vec(&self.settings) {
            Ok(b) => RVec::from(b),
            Err(_) => RVec::new(),
        }
    }

    fn apply_settings(&mut self, settings_json: RStr<'_>) {
        if let Ok(s) = serde_json::from_str::<Settings>(settings_json.as_str()) {
            self.settings = s;
            self.refresh_ui();
        }
    }

    fn initialize(&mut self, host_api: HostApi) {
        self.host_api = Some(host_api.clone());
        host_api.info("S-101 Explorer initialised");
        if let Some(json) = host_api.load_plugin_config(PLUGIN_ID) {
            if let Ok(s) = serde_json::from_str::<Settings>(&json) {
                self.settings = s;
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(api) = &self.host_api {
            if let Ok(json) = serde_json::to_string(&self.settings) {
                let _ = api.save_plugin_config(PLUGIN_ID, &json);
            }
        }
    }

    fn export_data(&self) -> ROption<RVec<u8>> {
        ROption::RNone
    }
    fn import_data(&mut self, _data: RStr<'_>) -> bool {
        false
    }
    fn clear(&mut self) {
        self.last_result = None;
        self.last_op = None;
    }
}

extern "C" fn create_plugin() -> Plugin_TO<'static, RBox<()>> {
    Plugin_TO::from_value(ExplorerPlugin::new(), TD_Opaque)
}

static MODULE: OnceLock<PluginModule> = OnceLock::new();

#[no_mangle]
pub extern "C" fn get_plugin_module() -> &'static PluginModule {
    MODULE.get_or_init(|| PluginModule {
        create_plugin,
        api_version: PLUGIN_API_VERSION,
        min_host_version: RString::from("1.0.0"),
        metadata: PluginMetadata {
            id: RString::from(PLUGIN_ID),
            name: RString::from(PLUGIN_NAME),
            version: RString::from(PLUGIN_VERSION),
            author: RString::from("FerriteS100 Team"),
            description: RString::from(
                "In-process index browser for S-101 ENC data using HostApi chart_* queries.",
            ),
        },
    })
}
