//! Dataset bindings for prepared drawing passes. Only masked decisions upload
//! a texture; point-origin Unclipped decisions retain the ordinary pipelines.
use crate::coverage_clip::{ClipTransform, CoverageClip};
use crate::{Result, WgpuError};
use ferrite_kernel::coverage_frame::FrameCoverageDecision;
use ferrite_kernel::coverage_raster::PixelMask;
use ferrite_render::PreparedCoverage;
use std::collections::{BTreeMap, BTreeSet};

pub struct CoverageGpuPlan<'a> {
    revision: u64,
    view_revision: u64,
    instruction_count: usize,
    passes: usize,
    masks: Vec<(usize, usize, &'a PixelMask)>,
    pixel_bytes: usize,
}
impl<'a> CoverageGpuPlan<'a> {
    /// Validate the whole upload before allocating GPU resources. Budgets are
    /// logical R8 texels; uniforms, driver overhead and prior in-flight frames
    /// are separate renderer accounting, not measured RSS or VRAM.
    pub fn new(
        prepared: &'a PreparedCoverage,
        revision: u64,
        view_revision: u64,
        instruction_count: usize,
        pass_count: usize,
        texture_limit: u32,
        pixel_budget: usize,
    ) -> Result<Self> {
        let invalid = |text: &str| WgpuError::Render(text.into());
        prepared
            .validate(revision, view_revision, instruction_count)
            .map_err(|error| invalid(&error.to_string()))?;
        if pass_count == 0 || prepared.pass(pass_count).is_ok() {
            return Err(invalid("Coverage projection pass count mismatch"));
        }
        let mut masks = Vec::new();
        let mut pixel_bytes = 0usize;
        for pass in 0..pass_count {
            let source = prepared.pass(pass).map_err(|e| invalid(&e.to_string()))?;
            if source.decision(instruction_count).is_ok() {
                return Err(invalid("Coverage instruction count mismatch"));
            }
            let mut datasets = BTreeSet::new();
            for index in 0..instruction_count {
                if let FrameCoverageDecision::ClipDataset(id) = source
                    .decision(index)
                    .map_err(|e| invalid(&e.to_string()))?
                {
                    datasets.insert(id);
                }
            }
            for dataset in datasets {
                let mask = source
                    .frame()
                    .mask(dataset)
                    .ok_or_else(|| invalid("Missing dataset coverage mask"))?;
                let size = mask.size();
                let image = if size.contains(&0) { [1, 1] } else { size };
                if image.iter().any(|n| *n > texture_limit)
                    || mask.origin().iter().any(|v| *v > 1 << 24)
                {
                    return Err(invalid("Coverage texture dimensions exceed device limits"));
                }
                let bytes = (image[0] as usize)
                    .checked_mul(image[1] as usize)
                    .ok_or_else(|| invalid("Coverage texture size overflow"))?;
                pixel_bytes = pixel_bytes
                    .checked_add(bytes)
                    .ok_or_else(|| invalid("Coverage upload size overflow"))?;
                if pixel_bytes > pixel_budget {
                    return Err(invalid(
                        "Coverage frame textures exceed aggregate pixel budget",
                    ));
                }
                masks.push((pass, dataset, mask));
            }
        }
        Ok(Self {
            revision,
            view_revision,
            instruction_count,
            passes: pass_count,
            masks,
            pixel_bytes,
        })
    }
    pub fn pixel_bytes(&self) -> usize {
        self.pixel_bytes
    }
    pub fn mask_count(&self) -> usize {
        self.masks.len()
    }
}

pub enum CoverageGpuBinding<'a> {
    Unclipped,
    Hidden,
    Masked(&'a wgpu::BindGroup),
}
pub struct CoverageGpuFrame {
    revision: u64,
    view_revision: u64,
    instruction_count: usize,
    passes: usize,
    masks: BTreeMap<(usize, usize), CoverageClip>,
    pixel_bytes: usize,
}
impl CoverageGpuFrame {
    pub fn upload(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        plan: CoverageGpuPlan<'_>,
    ) -> Result<Self> {
        // The caller may have planned against adapter limits rather than the
        // requested device limits. Reject all mismatches before any allocation.
        let limit = device.limits().max_texture_dimension_2d;
        for (_, _, mask) in &plan.masks {
            let size = mask.size();
            let image = if size.contains(&0) { [1, 1] } else { size };
            if image.iter().any(|dimension| *dimension > limit) {
                return Err(WgpuError::Render(
                    "Coverage upload exceeds requested device limits".into(),
                ));
            }
        }
        let mut masks = BTreeMap::new();
        let mut allocated = 0usize;
        for (pass, dataset, mask) in plan.masks {
            let clip = CoverageClip::new(
                device,
                queue,
                layout,
                Some(mask),
                ClipTransform::IDENTITY,
                plan.pixel_bytes - allocated,
            )?;
            allocated += clip.pixel_bytes();
            masks.insert((pass, dataset), clip);
        }
        Ok(Self {
            revision: plan.revision,
            view_revision: plan.view_revision,
            instruction_count: plan.instruction_count,
            passes: plan.passes,
            masks,
            pixel_bytes: allocated,
        })
    }
    pub fn resolve(
        &self,
        pass: usize,
        decision: FrameCoverageDecision,
    ) -> Result<CoverageGpuBinding<'_>> {
        if pass >= self.passes {
            return Err(WgpuError::Render("Coverage draw pass outside frame".into()));
        }
        Ok(match decision {
            FrameCoverageDecision::Unclipped => CoverageGpuBinding::Unclipped,
            FrameCoverageDecision::Hidden => CoverageGpuBinding::Hidden,
            FrameCoverageDecision::ClipDataset(dataset) => CoverageGpuBinding::Masked(
                &self
                    .masks
                    .get(&(pass, dataset))
                    .ok_or_else(|| {
                        WgpuError::Render("No coverage binding for dataset draw".into())
                    })?
                    .bind_group,
            ),
        })
    }
    /// Resolve only from a binding for this instruction geometry and view.
    /// The renderer obtains `prepared` through the context's stale-view checks.
    pub fn resolve_instruction(
        &self,
        prepared: &PreparedCoverage,
        pass: usize,
        index: usize,
    ) -> Result<CoverageGpuBinding<'_>> {
        prepared
            .validate(self.revision, self.view_revision, self.instruction_count)
            .map_err(|error| WgpuError::Render(error.to_string()))?;
        let decision = prepared
            .pass(pass)
            .and_then(|source| source.decision(index))
            .map_err(|error| WgpuError::Render(error.to_string()))?;
        self.resolve(pass, decision)
    }
    /// Inverse affine transform from current fragments to the retained mask.
    /// Non-affine camera/projection changes require a newly prepared frame.
    pub fn set_transform(&mut self, queue: &wgpu::Queue, transform: ClipTransform) {
        for mask in self.masks.values_mut() {
            mask.set_transform(queue, transform);
        }
    }
    pub fn pixel_bytes(&self) -> usize {
        self.pixel_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::coverage_frame::CoverageFrame;
    use ferrite_kernel::coverage_selection::{
        CoverageFootprint, Region, SelectedCoverage, Selection,
    };
    use ferrite_kernel::scale_policy::CoverageScaleRange;
    use ferrite_render::{
        DrawingInstruction, InstructionCoverageClass, PointInstruction, PortrayalOrigin,
        PreparedCoveragePass, WorldPoint,
    };
    use std::sync::Arc;
    fn prepared(extent: [u32; 2], point_only: bool) -> PreparedCoverage {
        let rect = |w| {
            Region::from_rings(&[[0., 0.], [w, 0.], [w, 10.], [0., 10.], [0., 0.]], &[]).unwrap()
        };
        let viewport = rect(10.);
        let inventory = [(180000, 10.), (45000, 5.)]
            .into_iter()
            .enumerate()
            .map(|(id, (min, w))| CoverageFootprint {
                dataset_id: id,
                coverage_id: id as i64,
                region: rect(w),
                scales: CoverageScaleRange {
                    minimum_denominator: Some(min),
                    optimum_denominator: min / 2,
                    maximum_denominator: min / 4,
                },
            })
            .collect::<Vec<_>>();
        let selection = Selection {
            display_band: 10,
            coverages: (0..2)
                .map(|i| SelectedCoverage {
                    inventory_index: i,
                    selection_band: 10,
                    selected_to_fill_gap: false,
                })
                .collect(),
            uncovered: Region::from_polygons(vec![]).unwrap(),
        };
        let frame =
            Arc::new(CoverageFrame::new(&inventory, &selection, &viewport, extent, 4096).unwrap());
        let mut instruction = DrawingInstruction::Point(PointInstruction::new(
            "symbol".into(),
            WorldPoint::new(1., 1.),
        ));
        instruction.set_portrayal_origin(if point_only {
            PortrayalOrigin::feature_point(WorldPoint::new(11., 1.)).unwrap()
        } else {
            PortrayalOrigin::NonPoint
        });
        let instructions = vec![instruction.clone(), instruction];
        let passes = (0..2)
            .map(|_| {
                PreparedCoveragePass::prepare(
                    &instructions,
                    frame.clone(),
                    |_| Ok(InstructionCoverageClass::Dataset(0)),
                    |_, _| Ok(Some([11., 1.])),
                )
                .unwrap()
            })
            .collect();
        PreparedCoverage::new(1, 2, 2, passes).unwrap()
    }
    #[test]
    fn duplicate_commands_upload_one_mask_per_dataset_and_projection_pass() {
        let prepared = prepared([12, 12], false);
        let plan = CoverageGpuPlan::new(&prepared, 1, 2, 2, 2, 8192, 200).unwrap();
        assert!(CoverageGpuPlan::new(&prepared, 0, 2, 2, 2, 8192, 200).is_err());
        assert!(CoverageGpuPlan::new(&prepared, 1, 3, 2, 2, 8192, 200).is_err());
        assert_eq!(plan.mask_count(), 2); // four commands, only two distinct bindings
        assert_eq!(plan.pixel_bytes(), 200);
        assert!(CoverageGpuPlan::new(&prepared, 1, 2, 2, 2, 8192, 199).is_err());
        assert!(CoverageGpuPlan::new(&prepared, 1, 2, 2, 2, 9, 200).is_err());
        assert!(CoverageGpuPlan::new(&prepared, 1, 2, 1, 2, 8192, 200).is_err());
        assert!(CoverageGpuPlan::new(&prepared, 1, 2, 3, 2, 8192, 200).is_err());
        assert!(CoverageGpuPlan::new(&prepared, 1, 2, 2, 1, 8192, 200).is_err());
        assert!(CoverageGpuPlan::new(&prepared, 1, 2, 2, 3, 8192, 200).is_err());
    }
    #[test]
    fn empty_masks_use_one_texel_and_unclipped_points_need_no_mask_texture() {
        let empty = prepared([0, 0], false);
        let plan = CoverageGpuPlan::new(&empty, 1, 2, 2, 2, 8192, 2).unwrap();
        assert_eq!(plan.pixel_bytes(), 2);
        assert!(CoverageGpuPlan::new(&empty, 1, 2, 2, 2, 8192, 1).is_err());
        let points = prepared([12, 12], true);
        let plan = CoverageGpuPlan::new(&points, 1, 2, 2, 2, 8192, 0).unwrap();
        assert_eq!(plan.pixel_bytes(), 0);
        assert_eq!(plan.mask_count(), 0);
    }
}
