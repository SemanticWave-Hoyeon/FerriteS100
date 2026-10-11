//! ABI-compatible external wrapper over the same native route controller.
use abi_stable::{
    sabi_trait::TD_Opaque,
    std_types::{RBox, RString},
};
use ferrite_plugin_api::{PluginMetadata, PluginModule, Plugin_TO, PLUGIN_API_VERSION};
pub use ferrite_s421::*;
use std::sync::OnceLock;
// Plugin module export

static PLUGIN_MODULE: OnceLock<PluginModule> = OnceLock::new();

fn init_plugin_module() -> PluginModule {
    PluginModule {
        create_plugin,
        api_version: PLUGIN_API_VERSION,
        min_host_version: RString::from("0.2.0"),
        metadata: PluginMetadata {
            id: RString::from(PLUGIN_ID),
            name: RString::from(PLUGIN_NAME),
            version: RString::from(PLUGIN_VERSION),
            author: RString::from("FerriteS100 Team"),
            description: RString::from(
                "S-421 Route Planning - Add waypoints, measure distances, export routes",
            ),
        },
    }
}

extern "C" fn create_plugin() -> Plugin_TO<'static, RBox<()>> {
    Plugin_TO::from_value(RoutePlugin::new(), TD_Opaque)
}

#[no_mangle]
pub extern "C" fn get_plugin_module() -> &'static PluginModule {
    PLUGIN_MODULE.get_or_init(init_plugin_module)
}
