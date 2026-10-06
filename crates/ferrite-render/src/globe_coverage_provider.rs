//! Product policy is supplied by the host; render backends own camera and pixels.
use crate::{PreparedCoverage, RenderContext, Result};
use ferrite_kernel::globe_camera::GlobeCamera;

/// Exact chart-pane projection used by both rendering and picking. Extent and
/// coordinates are local physical pixels, never global window coordinates.
pub struct GlobeCoverageView<'a> {
    pub camera: &'a GlobeCamera,
    pub extent: [u32; 2],
    pub display_scale: f64,
    pub pixels_per_mm: f64,
}

pub trait GlobeCoverageProvider: Send + Sync {
    /// Context is sorted and has a fresh coverage view revision. Return None
    /// only for an explicitly exempt inventory, never to recover from an error.
    fn prepare(
        &self,
        context: &RenderContext,
        view: GlobeCoverageView<'_>,
    ) -> Result<Option<PreparedCoverage>>;
}

impl GlobeCoverageView<'_> {
    /// Prepare one immutable binding and reject missing policy or stale masks.
    pub fn bind(
        &self,
        context: &mut RenderContext,
        provider: Option<&dyn GlobeCoverageProvider>,
    ) -> Result<Option<std::sync::Arc<PreparedCoverage>>> {
        if let Some(provider) = provider {
            context.require_prepared_coverage();
            if self.extent.contains(&0) || !self.display_scale.is_finite() || self.display_scale <= 0.
                || !self.pixels_per_mm.is_finite() || self.pixels_per_mm <= 0. {
                return Err(crate::RenderError::Render("Invalid globe coverage device metrics".into()));
            }
            match provider.prepare(context, GlobeCoverageView { camera: self.camera, ..*self })? {
                Some(prepared) => {
                    if prepared.pass_count() != 1 {
                        return Err(crate::RenderError::Render("Globe coverage requires exactly one camera projection".into()));
                    }
                    context.set_prepared_coverage(prepared)?;
                }
                None => context.clear_prepared_coverage(),
            }
        } else if context.prepared_coverage_binding()?.is_some() {
            return Err(crate::RenderError::Render("Globe coverage policy missing for a coverage-bound chart".into()));
        }
        context.prepared_coverage_binding()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RenderError, Viewport};
    struct Provider { fail: bool }
    impl GlobeCoverageProvider for Provider {
        fn prepare(&self, _: &RenderContext, _: GlobeCoverageView<'_>) -> Result<Option<PreparedCoverage>> {
            if self.fail { Err(RenderError::Render("projector failed".into())) } else { Ok(None) }
        }
    }
    fn camera() -> GlobeCamera {
        GlobeCamera::orbit(ferrite_kernel::geodesy::GeographicPosition::new(48., 0.).unwrap(),
            30000., 0., 45., [640., 480.], 45., 3., 1e9).unwrap()
    }
    #[test]
    fn failed_projection_stays_required_and_cannot_fall_back_without_policy() {
        let camera = camera();
        let view = GlobeCoverageView { camera: &camera, extent: [640, 480], display_scale: 90000., pixels_per_mm: 4. };
        let mut context = RenderContext::new(Viewport::new(640., 480.));
        let revision = context.coverage_view_revision();
        assert!(view.bind(&mut context, Some(&Provider { fail: true })).unwrap_err().to_string().contains("projector failed"));
        assert_ne!(context.coverage_view_revision(), revision);
        assert!(view.bind(&mut context, None).is_err());
        assert!(view.bind(&mut context, Some(&Provider { fail: false })).unwrap().is_none());
        assert!(view.bind(&mut context, None).unwrap().is_none());
    }
    #[test]
    fn invalid_device_metrics_do_not_authorize_an_exempt_fallback() {
        let camera = camera();
        for (extent, scale, ppm) in [([0,480], 90000.,4.), ([640,480],f64::NAN,4.), ([640,480],90000.,0.)] {
            let mut context = RenderContext::new(Viewport::new(640.,480.));
            let view = GlobeCoverageView { camera: &camera, extent, display_scale: scale, pixels_per_mm: ppm };
            assert!(view.bind(&mut context, Some(&Provider { fail: false })).is_err());
            assert!(context.prepared_coverage_binding().is_err());
        }
    }
}
