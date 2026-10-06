//! Shared point/text adapter for geographic and absolute device origins.
use ferrite_kernel::{geodesy::GeographicPosition, globe_camera::GlobeCamera, portrayal_position::PortrayalDevice};
use ferrite_render::{PortrayalOrigin, WorldPoint};
pub(crate) struct GlobePointAnchor {
    pub ecef_m: [f64;3],
    pub screen_px: [f64;2],
    geographic: Option<GeographicPosition>,
}
impl GlobePointAnchor {
    pub fn resolve(origin: &PortrayalOrigin, position: WorldPoint, camera: &GlobeCamera, pixels_per_mm: f64) -> Result<Option<Self>,String> {
        if origin.is_device_fixed() {
            let device = PortrayalDevice::new([0.,camera.viewport()[1]], [pixels_per_mm,pixels_per_mm]).map_err(|e|e.to_string())?;
            let screen_px = origin.device_pixel_position(device).map_err(|e|e.to_string())?.ok_or("Missing device point")?;
            let ecef_m = camera.device_plane_point(screen_px).map_err(|e|e.to_string())?;
            return Ok(Some(Self{ecef_m,screen_px,geographic:None}));
        }
        let g = GeographicPosition::new(position.y,(position.x+180.).rem_euclid(360.)-180.).map_err(|e|e.to_string())?;
        let ecef_m = g.to_ecef(0.).map_err(|e|e.to_string())?;
        Ok(camera.project_visible(ecef_m).map_err(|e|e.to_string())?.map(|p|Self{ecef_m,screen_px:p.screen_px,geographic:Some(g)}))
    }
    pub fn project_bearing(&self,camera:&GlobeCamera,bearing:f64)->Result<[f64;2],String> {
        let g=if let Some(g)=self.geographic {g} else {
            camera.pick(self.screen_px).map_err(|e|e.to_string())?.ok_or("Geographic rotation at device point has no visible Earth basis")?.geodetic.surface
        };
        camera.project_bearing(g,bearing).map_err(|e|e.to_string())
    }
}
