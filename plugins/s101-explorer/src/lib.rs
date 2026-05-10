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
    /// MCP server registration info so users can register the same
    /// in-process tools with an external LLM client (Claude Desktop,
    /// ChatGPT, etc). `None` until a chart is loaded — without an
    /// active chart there's nothing to register.
    connection_info: Option<ConnectionInfo>,
}

#[derive(Debug, Serialize)]
struct ConnectionInfo {
    /// Path to the s101-mcp executable that the LLM client will spawn.
    /// `binary_found` is true if the path exists on disk; false means
    /// the user needs to build the binary or edit settings.
    binary_path: String,
    binary_found: bool,
    /// Chart file the server should pre-load.
    chart_path: String,
    /// Feature catalogue (file or directory).
    catalogue_path: String,
    /// Full command-line as a copy-pasteable shell string.
    command_line: String,
    /// `claude_desktop_config.json` snippet — the canonical MCP-client
    /// configuration shape. Same JSON works for OpenAI / OpenRouter / etc.
    config_snippet: String,
}

/// Find the s101-mcp executable. Probed in order:
///   1. `s101-mcp.exe` next to the host (self-contained dist)
///   2. `plugins/s101-mcp/target/debug/s101-mcp.exe` (cargo run dev)
///   3. `plugins/s101-mcp/target/release/s101-mcp.exe` (release source)
///
/// Returns the first existing candidate, or candidate #1 as the best
/// guess if none are found (so the panel still has something to show).
fn probe_binary_path() -> (String, bool) {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            "s101-mcp.exe",
            "plugins/s101-mcp/target/release/s101-mcp.exe",
            "plugins/s101-mcp/target/debug/s101-mcp.exe",
        ]
    } else {
        &[
            "s101-mcp",
            "plugins/s101-mcp/target/release/s101-mcp",
            "plugins/s101-mcp/target/debug/s101-mcp",
        ]
    };
    for c in candidates {
        if std::path::Path::new(c).exists() {
            // Canonicalize so the snippet shows an absolute path that
            // will resolve from any CWD an LLM client launches us in.
            let abs = std::fs::canonicalize(c)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| c.to_string());
            return (strip_unc_prefix(&abs), true);
        }
    }
    (candidates[0].to_string(), false)
}

/// On Windows, `canonicalize` returns paths with a `\\?\` UNC prefix
/// that confuses some LLM client config parsers. Strip it.
fn strip_unc_prefix(s: &str) -> String {
    s.strip_prefix(r"\\?\").unwrap_or(s).to_string()
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() || s.contains(char::is_whitespace) || s.contains('"') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Build the `ConnectionInfo` from a `dataset_metadata` JSON response and
/// a probed binary path. Returns None if either chart_path or
/// catalogue_path are missing (i.e. the host hasn't fully wired things up
/// yet — defensive against partial state).
fn build_connection_info(metadata: &serde_json::Value) -> Option<ConnectionInfo> {
    let chart_path = metadata
        .get("dataset")
        .and_then(|d| d.get("file"))
        .and_then(|v| v.as_str())?
        .to_string();
    let catalogue_path = metadata
        .get("catalogue_path")
        .and_then(|v| v.as_str())
        .unwrap_or("Catalogues/FC/S-101")
        .to_string();
    let (binary_path, binary_found) = probe_binary_path();

    let command_line = format!(
        "{} --chart {} --catalogue {}",
        shell_quote(&binary_path),
        shell_quote(&chart_path),
        shell_quote(&catalogue_path)
    );

    let config = serde_json::json!({
        "mcpServers": {
            "s101": {
                "command": binary_path,
                "args": [
                    "--chart", chart_path,
                    "--catalogue", catalogue_path
                ]
            }
        }
    });
    let config_snippet = serde_json::to_string_pretty(&config).unwrap_or_default();

    Some(ConnectionInfo {
        binary_path: shell_quote_inner(&binary_path),
        binary_found,
        chart_path,
        catalogue_path,
        command_line,
        config_snippet,
    })
}

// shell_quote, but we use it for storing the raw binary path back into
// the struct unquoted (UI shows it as plain text). Keeping a separate
// helper so the call site stays honest about which is which.
fn shell_quote_inner(s: &str) -> String {
    s.to_string()
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

        // Pull metadata once and reuse for the summary line + connection
        // info. Avoids a second HostApi round-trip per redraw.
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

        let connection_info = metadata.as_ref().and_then(build_connection_info);

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
