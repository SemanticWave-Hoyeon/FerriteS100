//! The S-421 stage of a host open job ([`ferrite_product_api::open`]):
//! routes recognised by name and head ([`super::S421`]) are captured while
//! the job classifies its inputs (refused by the signature policy: a
//! notice), then inserted into the layer (moved from the engine's
//! `ChartEngine` open job and install).
use super::input::{capture_if_route_in, CapturedRouteInput};
use super::{S421Layer, S421};
use ferrite_product_api::open::{Loaded, OpenContext, OpenNotes, OpenStage};
use ferrite_product_api::report::InstallReport;
use ferrite_product_api::{Opened, ProductAdapter, Recognised};
use std::any::Any;
use std::path::Path;

/// The captured routes of a job.
#[derive(Default)]
pub struct S421Stage {
    routes: Vec<CapturedRouteInput>,
}

impl OpenStage for S421Stage {
    fn sniffs_head(&self, name: &Path) -> bool {
        S421::is_route_name(name)
    }
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised> {
        S421.sniff(name, head)
    }
    /// Capture the route once; capture failures and routes refused by the
    /// signature policy are notices (the input is then not taken).
    fn accept(&mut self, path: &Path, context: &OpenContext<'_>, notes: &mut OpenNotes) -> bool {
        match capture_if_route_in(context.data, path) {
            Ok(Some(route)) => match route.allowed_xml(context.required()) {
                Ok(_) => {
                    self.routes.push(route);
                    true
                }
                Err(error) => {
                    notes
                        .notices
                        .push(format!("Not opened: {}: {error}", path.display()));
                    false
                }
            },
            Ok(None) => false,
            Err(error) => {
                notes
                    .notices
                    .push(format!("Not opened: {}: {error}", path.display()));
                false
            }
        }
    }
    fn load(self: Box<Self>, _: &OpenContext<'_>, _: &mut OpenNotes) -> anyhow::Result<Loaded> {
        let counts = vec![("routes", self.routes.len())];
        Ok(Loaded {
            payload: Box::new(self.routes),
            counts,
        })
    }
}

/// Insert the captured routes (the same SHA-256 is "Already open").
pub(crate) fn install(
    layer: &mut S421Layer,
    opened: &mut (dyn Any + Send),
    report: &mut InstallReport,
) -> anyhow::Result<()> {
    let routes = opened
        .downcast_mut::<Vec<CapturedRouteInput>>()
        .ok_or_else(|| anyhow::anyhow!("Not an S-421 open result"))?;
    for route in std::mem::take(routes) {
        let path = route.path().to_path_buf();
        match layer.insert(route) {
            Opened::AlreadyOpen => report
                .notices
                .push(format!("Already open: {}", path.display())),
            Opened::New(_) => report.notices.push(format!(
                "S-421 route captured (not portrayed by ChartEngine): {}",
                path.display()
            )),
        }
    }
    Ok(())
}
