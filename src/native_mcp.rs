//! Native product-independent MCP publication of the host's loaded dataset tree.
use ferrite_mcp::{
    http::Server,
    service::{Dataset, Registry},
    Indices,
};
use serde_json::json;
use std::sync::Arc;
#[derive(Default)]
pub struct Controller {
    pub server: Option<Server>,
    signature: String,
    pub published: usize,
    error: Option<String>,
    pending: Option<std::sync::mpsc::Receiver<anyhow::Result<Registry>>>,
}
impl super::ChartApp {
    pub(super) fn service_mcp_ui(&mut self) {
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        if !renderer.ui_state.s100_mcp.open
            && !renderer.ui_state.s100_mcp.enabled
            && !renderer.ui_state.s100_mcp.restart
            && self.s100_mcp.server.is_none()
        {
            return;
        }
        let mut ui = std::mem::take(&mut renderer.ui_state.s100_mcp);
        if ui.restart {
            ui.restart = false;
            self.s100_mcp.server = None;
            self.s100_mcp.signature.clear();
            self.s100_mcp.published = 0;
            self.s100_mcp.error = None;
            self.s100_mcp.pending = None;
            if ui.enabled {
                match Server::start(ui.public_tunnel) {
                    Ok(s) => self.s100_mcp.server = Some(s),
                    Err(e) => {
                        ui.enabled = false;
                        ui.status = format!("Could not start service: {e}");
                    }
                }
            }
        }
        if self.s100_mcp.server.is_some() {
            // The tree contains only committed loaded datasets; it changes on load/unload or catalogue switch.
            let layers = self
                .renderer
                .as_ref()
                .unwrap()
                .ui_state
                .dataset_layers
                .clone();
            let signature = format!(
                "{layers:?}:{:p}:{:p}",
                Arc::as_ptr(&self.fc),
                Arc::as_ptr(&self.pc)
            );
            if (ui.refresh || signature != self.s100_mcp.signature)
                && self.s100_mcp.pending.is_some()
            {
                self.s100_mcp.server.as_ref().unwrap().clear();
                ui.refresh = true;
            }
            if (ui.refresh || signature != self.s100_mcp.signature)
                && self.s100_mcp.pending.is_none()
            {
                ui.refresh = false;
                // Invalidate old state synchronously before rebuilding so an unload cannot retain stale data.
                self.s100_mcp.server.as_ref().unwrap().clear();
                self.s100_mcp.pending = None;
                let mut datasets = Vec::new();
                for layer in &layers {
                    for file in &layer.files {
                        let index = if layer.product == "S-101" {
                            self.cells
                                .iter()
                                .find(|c| c.dsid.dataset_name == file.name)
                                .and_then(|cell| {
                                    // Never force an index onto an incompatible catalogue. The common metadata remains available.
                                    ferrite_s101::validate_dataset_catalogues(
                                        &cell.dsid,
                                        &self.fc,
                                        &self.pc.product_id,
                                        &self.pc.version,
                                    )
                                    .ok()?;
                                    Some(cell.clone())
                                })
                        } else {
                            None
                        };
                        datasets.push((index, Dataset{id:format!("{}:{}",layer.product,file.source),product:layer.product.clone(),metadata:json!({"name":file.name,"source":file.source,"detail":file.detail,"feature_catalogue":layer.fc,"portrayal_catalogue":layer.pc}),s101:None}));
                    }
                }
                self.s100_mcp.error = None;
                self.s100_mcp.signature = signature.clone();
                self.s100_mcp.published = 0;
                let fc = Arc::new((**self.fc).clone());
                let (sender, receiver) = std::sync::mpsc::channel();
                let launch = std::thread::Builder::new()
                    .name("ferrite-mcp-index".into())
                    .spawn(move || {
                        let built = datasets.into_iter().map(|(cell, mut dataset)| {
                            dataset.s101 =
                                cell.map(|cell| Arc::new(Indices::build_shared(cell, fc.clone())));
                            dataset
                        });
                        let _ = sender.send(Registry::new(built));
                    });
                match launch {
                    Ok(_) => self.s100_mcp.pending = Some(receiver),
                    Err(e) => {
                        self.s100_mcp.error = Some(format!("Index worker could not start: {e}"));
                    }
                }
            }
            let completed = self
                .s100_mcp
                .pending
                .as_ref()
                .and_then(|r| match r.try_recv() {
                    Ok(v) => Some(v),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(_) => Some(Err(anyhow::anyhow!("Index worker stopped"))),
                });
            if let Some(result) = completed {
                self.s100_mcp.pending = None;
                match result {
                    Ok(registry) if !ui.refresh && self.s100_mcp.signature == signature => {
                        self.s100_mcp.published = registry.len();
                        self.s100_mcp
                            .server
                            .as_ref()
                            .unwrap()
                            .set_registry(registry);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        self.s100_mcp.error = Some(format!("Dataset publication rejected: {e}"));
                    }
                }
            }
            let server = self.s100_mcp.server.as_ref().unwrap();
            let info = server.info();
            ui.url = info.public_url.unwrap_or(info.url);
            ui.status = if self.s100_mcp.pending.is_some() {
                "Preparing read-only query indexes…".into()
            } else {
                self.s100_mcp.error.clone().unwrap_or(info.status)
            };
            ui.approval_code = server.approval_code();
            ui.dataset_count = self.s100_mcp.published;
        } else {
            ui.url.clear();
            ui.approval_code.clear();
            ui.dataset_count = 0;
            if !ui.enabled {
                ui.status = "Service stopped".into();
            }
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.s100_mcp = ui;
        }
    }
}
