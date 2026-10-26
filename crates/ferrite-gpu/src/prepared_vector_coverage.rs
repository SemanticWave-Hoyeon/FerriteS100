//! FIRST separation only: actual fallible coverage/OVERSC01 resources.
//! Not a complete PreparedVectorFrame or a cancellation publication capability.
use crate::{Result, WgpuError};
use std::sync::Arc;

pub(crate) struct CoverageResources {
    pub(crate) frame: Option<crate::coverage_gpu_frame::CoverageGpuFrame>,
    pub(crate) pipelines: Option<Arc<crate::coverage_pipeline::CoveragePipelines>>,
    pub(crate) annotations: Vec<crate::overscale_annotation::OverscaleAnnotation>,
}

/// Shared normal/private implementation replacing the actual resource block.
/// Inputs preserve original material/palette/owner/coverage semantics and budgets.
/// A normal caller may pass immutable Arc pipeline handles; PRIVATE preparation
/// must supply PRIVATE caches/resources. This helper never receives renderer
/// geometry fields, the live frame, the live target, or mutable view uniforms.
#[expect(
    clippy::too_many_arguments,
    reason = "Exact independent source/view/device/layout/PC inputs; no policy is inferred or collapsed"
)]
pub(crate) fn prepare_coverage_resources(
    state: &crate::GpuState,
    pipelines: &crate::RenderPipelines,
    context: &ferrite_render::RenderContext,
    prepared: Option<&Arc<ferrite_render::PreparedCoverage>>,
    mut coverage_pipelines: Option<Arc<crate::coverage_pipeline::CoveragePipelines>>,
    symbol_cache: Option<&mut crate::SymbolCache>,
    color_profile: Option<&ferrite_portrayal_catalog::ColorProfile>,
    resource_owners: Option<&mut crate::CellPortrayalResources>,
    longitude_wrap_screen_px: f32,
    frame_local_clip_reuse: bool,
    program_reuse: Option<&crate::overscale_annotation::ProgramReuse>,
    mut work: Option<&mut crate::coverage_trial::Work>,
) -> Result<CoverageResources> {
    let frame = if let Some(prepared) = prepared {
        if let Some(work) = work.as_deref_mut() {
            work.plans = work.plans.saturating_add(1);
        }
        let plan = crate::coverage_gpu_frame::CoverageGpuPlan::new_with_frame_local_reuse(
            prepared,
            context.geometry_revision(),
            context.coverage_view_revision(),
            context.instruction_count(),
            if longitude_wrap_screen_px > 0. { 3 } else { 1 },
            state.device.limits().max_texture_dimension_2d,
            128 * 1024 * 1024,
            frame_local_clip_reuse,
        )?;
        if plan.mask_count() > 0 && coverage_pipelines.is_none() {
            coverage_pipelines = Some(Arc::new(
                crate::coverage_pipeline::CoveragePipelines::new_with_symbol_instancing(
                    &state.device,
                    state.format(),
                    crate::state::MSAA_SAMPLE_COUNT,
                    &pipelines.view_bind_group_layout,
                    &pipelines.texture_bind_group_layout,
                    &pipelines.pattern_bind_group_layout,
                    pipelines.symbol_instance_pipeline.is_some(),
                )?,
            ));
        }
        let fallback;
        let layout = if let Some(p) = &coverage_pipelines {
            &p.clip_layout
        } else {
            fallback = crate::coverage_clip::create_clip_layout(&state.device);
            &fallback
        };
        if let Some(work) = work {
            work.uploads = work.uploads.saturating_add(1);
            work.unique_mask_bytes = work
                .unique_mask_bytes
                .saturating_add(plan.unique_pixel_bytes() as u64);
        }
        Some(crate::coverage_gpu_frame::CoverageGpuFrame::upload(
            &state.device,
            &state.queue,
            layout,
            plan,
        )?)
    } else {
        None
    };

    let annotations = if let Some(prepared) = prepared {
        let mut annotations = Vec::new();
        for index in 0..prepared.pass_count() {
            if let Some(annotation) = prepared
                .pass(index)
                .map_err(|e| WgpuError::Render(e.to_string()))?
                .frame()
                .scale_annotations()
            {
                annotations.push(annotation);
            }
        }
        // Exactly the existing selected-gap + strict maximum predicate.
        if !annotations.iter().any(|a| {
            a.coverages.iter().any(|r| {
                r.selected_to_fill_gap
                    && a.viewing_denominator < f64::from(r.scales.maximum_denominator)
            })
        }) {
            Vec::new()
        } else if let Some(owners) = resource_owners {
            crate::overscale_annotation::OverscaleAnnotation::prepare_owned(
                &annotations,
                &context.scaler,
                owners,
                state,
                pipelines,
                program_reuse,
            )?
        } else {
            let symbols = symbol_cache.ok_or_else(|| {
                WgpuError::Render("OVERSC01 active-PC symbol cache missing".into())
            })?;
            let profile = color_profile
                .ok_or_else(|| WgpuError::Render("OVERSC01 active palette missing".into()))?;
            crate::overscale_annotation::OverscaleAnnotation::prepare(
                &annotations,
                &context.scaler,
                symbols,
                profile,
                state,
                pipelines,
                program_reuse,
            )?
            .into_iter()
            .collect()
        }
    } else {
        Vec::new()
    };
    Ok(CoverageResources {
        frame,
        pipelines: coverage_pipelines,
        annotations,
    })
}
