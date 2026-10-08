//! Native S-100 service controls; independent of the external plugin system.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub open: bool,
    pub enabled: bool,
    pub public_tunnel: bool,
    pub restart: bool,
    pub refresh: bool,
    pub status: String,
    pub url: String,
    pub approval_code: String,
    pub dataset_count: usize,
}
pub fn draw(ctx: &egui::Context, state: &mut State) {
    if !state.open {
        return;
    }
    egui::Window::new("S-100 MCP").open(&mut state.open).default_width(420.0).resizable(true).show(ctx,|ui|{
        ui.label("Read-only access to loaded S-100 datasets");
        if ui.checkbox(&mut state.enabled,"Enable MCP service").changed(){state.restart=true;}
        if ui.checkbox(&mut state.public_tunnel,"Expose through ngrok").changed(){state.restart=true;}
        ui.label("Local access is the default. Public access requires ngrok and its own account configuration.");
        ui.separator();ui.label(&state.status);
        if state.enabled&&!state.url.is_empty(){
            ui.horizontal(|ui|{ui.label("Endpoint");if ui.button("Copy URL").clicked(){ui.ctx().copy_text(state.url.clone());}});
            ui.add(egui::TextEdit::singleline(&mut state.url).desired_width(f32::INFINITY).interactive(false));
            ui.horizontal(|ui|{ui.label("Client authorization");if ui.button("Copy approval code").clicked(){ui.ctx().copy_text(state.approval_code.clone());}});
            ui.label("Paste this code into the authorization page only for a client you chose to connect. Stopping the service revokes its tokens.");
            ui.label(format!("{} datasets published",state.dataset_count));
            if ui.button("Refresh loaded datasets").clicked(){state.refresh=true;}
        }
        ui.collapsing("Supported queries",|ui|{
            ui.label("All loaded products: datasets, product and FC/PC metadata.");
            ui.label("S-101: catalogue search, attributes, feature lookup and approximate spatial queries.");
            ui.label("Other products currently expose metadata only. Queries do not modify charts, routes or portrayal.");
        });
    });
}
