//! Dataset bindings for prepared drawing passes. Only masked decisions upload
//! a texture; point-origin Unclipped decisions retain the ordinary pipelines.
use crate::coverage_clip::{ClipTransform, CoverageClip};
use crate::{Result, WgpuError};
use ferrite_kernel::coverage_frame::FrameCoverageDecision;
use ferrite_kernel::coverage_raster::PixelMask;
use ferrite_render::PreparedCoverage;
use std::collections::{BTreeMap, BTreeSet};

/// Shipping policy: absent or exact 1 enables frame-local reuse.
/// Exact 0, invalid and non-Unicode values preserve the original upload path.
/// Renderer samples this once during construction; no frame reads environment.
pub fn frame_local_clip_reuse_policy(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_none() || value == Some(std::ffi::OsStr::new("1"))
}

pub struct CoverageGpuPlan<'a> {
    revision: u64,
    view_revision: u64,
    instruction_count: usize,
    passes: usize,
    masks: Vec<(usize, usize, &'a PixelMask)>,
    pixel_bytes: usize,
    clip_indices: Option<Vec<usize>>,
    unique_pixel_bytes: usize,
    reuse_admitted: bool,
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
        Self::new_with_frame_local_reuse(prepared, revision, view_revision, instruction_count,
            pass_count, texture_limit, pixel_budget, false)
    }
    /// Reuse is limited to borrowed mask/frame identities in THIS new plan.
    /// Complete original validation and logical admission precede alias analysis.
    pub fn new_with_frame_local_reuse(
        prepared: &'a PreparedCoverage, revision: u64, view_revision: u64,
        instruction_count: usize, pass_count: usize, texture_limit: u32,
        pixel_budget: usize, reuse: bool,
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
        let (clip_indices, unique_pixel_bytes, reuse_admitted) =
            Self::frame_local_aliases(prepared, &masks, pixel_bytes, reuse);
        Ok(Self {
            revision,
            view_revision,
            instruction_count,
            passes: pass_count,
            masks,
            pixel_bytes,
            clip_indices,
            unique_pixel_bytes,
            reuse_admitted,
        })
    }
    fn frame_local_aliases(
        prepared: &PreparedCoverage, masks: &[(usize, usize, &PixelMask)],
        logical_bytes: usize, reuse: bool,
    ) -> (Option<Vec<usize>>, usize, bool) {
        // No sharing when admission declines: original cold upload, no drawing loss.
        if !reuse || masks.len() > 1024 {
            return (None, logical_bytes, false);
        }
        let mut seen = BTreeMap::new();
        let mut indices = Vec::with_capacity(masks.len());
        let mut unique_bytes = 0;
        for (pass, _, mask) in masks {
            // All passes were validated above. Keep a transparent cold fallback
            // if future planner changes invalidate that invariant.
            let Ok(source) = prepared.pass(*pass) else {
                return (None, logical_bytes, false);
            };
            let key = (source.frame() as *const _ as usize, *mask as *const PixelMask as usize);
            let index = match seen.get(&key) {
                Some(index) => *index,
                None => {
                    let index = seen.len();
                    seen.insert(key, index);
                    unique_bytes += Self::texture_bytes(mask);
                    index
                }
            };
            indices.push(index);
        }
        (Some(indices), unique_bytes, true)
    }
    fn texture_bytes(mask: &PixelMask) -> usize {
        let size = mask.size();
        let image = if size.contains(&0) { [1, 1] } else { size };
        // Checked for every alias during original admission, subset sum <= logical charge.
        image[0] as usize * image[1] as usize
    }
    fn validate_upload_limit(&self, limit: u32) -> Result<()> {
        for (_, _, mask) in &self.masks {
            let size = mask.size();
            let image = if size.contains(&0) { [1, 1] } else { size };
            if image.iter().any(|dimension| *dimension > limit) {
                return Err(WgpuError::Render("Coverage upload exceeds requested device limits".into()));
            }
        }
        Ok(())
    }
    pub fn unique_pixel_bytes(&self) -> usize { self.unique_pixel_bytes }
    pub fn unique_clip_count(&self) -> usize {
        self.clip_indices.as_ref().map_or(self.masks.len(), |indices| indices.iter().copied().max().map_or(0, |last| last + 1))
    }
    pub fn reuse_admitted(&self) -> bool { self.reuse_admitted }
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
    masks: BTreeMap<(usize, usize), usize>,
    clips: Vec<CoverageClip>,
    pixel_bytes: usize,
    unique_pixel_bytes: usize,
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
        plan.validate_upload_limit(device.limits().max_texture_dimension_2d)?;
        let mut masks = BTreeMap::new();
        let mut clips = Vec::new();
        let mut logical_allocated = 0usize;
        let mut unique_allocated = 0usize;
        for (alias, (pass, dataset, mask)) in plan.masks.into_iter().enumerate() {
            let index = plan.clip_indices.as_ref().map_or(alias, |indices| indices[alias]);
            if index == clips.len() {
                let clip = CoverageClip::new(
                    device, queue, layout, Some(mask), ClipTransform::IDENTITY,
                    plan.pixel_bytes - logical_allocated,
                )?;
                unique_allocated += clip.pixel_bytes();
                clips.push(clip);
            }
            // Charge EVERY alias exactly as the original uploader did, even
            // though the borrowed identity permits one physical texture.
            logical_allocated += CoverageGpuPlan::texture_bytes(mask);
            masks.insert((pass, dataset), index);
        }
        Ok(Self {
            revision: plan.revision,
            view_revision: plan.view_revision,
            instruction_count: plan.instruction_count,
            passes: plan.passes,
            masks,
            clips,
            pixel_bytes: logical_allocated,
            unique_pixel_bytes: unique_allocated,
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
            FrameCoverageDecision::ClipDataset(dataset) => {
                let index = self.masks.get(&(pass, dataset)).ok_or_else(|| {
                    WgpuError::Render("No coverage binding for dataset draw".into())
                })?;
                CoverageGpuBinding::Masked(&self.clips[*index].bind_group)
            },
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
        for mask in &mut self.clips {
            mask.set_transform(queue, transform);
        }
    }
    pub fn pixel_bytes(&self) -> usize {
        self.pixel_bytes
    }
    /// Physical logical R8 texels of unique clips; not total RSS/driver memory.
    pub fn unique_pixel_bytes(&self) -> usize { self.unique_pixel_bytes }
    pub fn unique_clip_count(&self) -> usize { self.clips.len() }
    pub fn mask_count(&self) -> usize { self.masks.len() }
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
        prepared_frames(extent, point_only, false, 2)
    }
    fn prepared_frames(extent: [u32; 2], point_only: bool, distinct: bool, passes: usize) -> PreparedCoverage {
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
        let passes = (0..passes)
            .map(|_| {
                PreparedCoveragePass::prepare(
                    &instructions,
                    if distinct { Arc::new(CoverageFrame::new(&inventory, &selection, &viewport, extent, 4096).unwrap()) } else { frame.clone() },
                    |_| Ok(InstructionCoverageClass::Dataset(0)),
                    |_, _| Ok(Some([11., 1.])),
                )
                .unwrap()
            })
            .collect();
        PreparedCoverage::new(1, 2, 2, passes).unwrap()
    }
    #[test]
    fn shipping_policy_defaults_on_and_exact_zero_opts_out() {
        use std::ffi::OsStr;
        assert!(frame_local_clip_reuse_policy(None));
        assert!(frame_local_clip_reuse_policy(Some(OsStr::new("1"))));
        assert!(!frame_local_clip_reuse_policy(Some(OsStr::new("0"))));
        // Public compatibility constructor remains original/non-reuse regardless of policy.
        let p = prepared_frames([12,12], false, false, 3);
        let baseline = CoverageGpuPlan::new(&p,1,2,2,3,8192,300).unwrap();
        assert_eq!(baseline.unique_clip_count(),3);
        assert_eq!(baseline.pixel_bytes(),300);
    }
    #[test]
    fn shipping_policy_invalid_and_nonunicode_values_keep_original_path() {
        use std::ffi::OsStr;
        for value in ["", "true", "false", " 1", "1 ", "01", "2", " 0", "0 "] {
            assert!(!frame_local_clip_reuse_policy(Some(OsStr::new(value))));
        }
        #[cfg(unix)] {
            use std::os::unix::ffi::OsStrExt;
            assert!(!frame_local_clip_reuse_policy(Some(OsStr::from_bytes(&[255]))));
        }
    }
    #[test]
    fn live_frame_identity_shares_three_passes_but_distinct_frames_do_not() {
        for distinct in [false, true] {
            let p = prepared_frames([12,12], false, distinct, 3);
            let on = CoverageGpuPlan::new_with_frame_local_reuse(&p,1,2,2,3,8192,300,true).unwrap();
            let off = CoverageGpuPlan::new(&p,1,2,2,3,8192,300).unwrap();
            assert_eq!(on.mask_count(), off.mask_count());
            assert_eq!(on.pixel_bytes(), 300);
            assert_eq!(on.unique_clip_count(), if distinct {3} else {1});
            assert_eq!(on.unique_pixel_bytes(), if distinct {300} else {100});
            assert_eq!(off.unique_clip_count(), 3);
            assert_eq!(on.clip_indices, Some(if distinct {vec![0,1,2]} else {vec![0,0,0]}));
            assert!(off.clip_indices.is_none());
        }
    }
    #[test]
    fn reuse_preserves_budget_revision_count_and_device_limit_errors() {
        let p=prepared_frames([12,12],false,false,3);
        for (revision,view,count,passes,limit,budget) in [(1,2,2,3,8192,299),(0,2,2,3,8192,300),(1,0,2,3,8192,300),(1,2,1,3,8192,300),(1,2,2,2,8192,300),(1,2,2,3,9,300)] {
            let err=|reuse|CoverageGpuPlan::new_with_frame_local_reuse(&p,revision,view,count,passes,limit,budget,reuse).err().unwrap().to_string();
            assert_eq!(err(false),err(true));
        }
        for reuse in [false,true] {
            let plan=CoverageGpuPlan::new_with_frame_local_reuse(&p,1,2,2,3,8192,300,reuse).unwrap();
            assert!(plan.validate_upload_limit(9).is_err());
            assert!(plan.validate_upload_limit(10).is_ok());
        }
    }
    #[test]
    fn empty_and_unclipped_aliases_keep_original_logical_admission() {
        for (point,logical,unique) in [(false,3,1),(true,0,0)] {
            let p=prepared_frames([0,0],point,false,3);
            let plan=CoverageGpuPlan::new_with_frame_local_reuse(&p,1,2,2,3,8192,logical,true).unwrap();
            assert_eq!(plan.pixel_bytes(),logical);assert_eq!(plan.unique_pixel_bytes(),unique);
        }
    }
    #[test]
    fn bounded_alias_admission_declines_to_complete_cold_plan() {
        let p=prepared_frames([12,12],false,false,1025);
        let plan=CoverageGpuPlan::new_with_frame_local_reuse(&p,1,2,2,1025,8192,102500,true).unwrap();
        assert!(!plan.reuse_admitted());assert_eq!(plan.unique_clip_count(),1025);
        assert_eq!(plan.pixel_bytes(),plan.unique_pixel_bytes());
        assert!(plan.clip_indices.is_none());
        let p=prepared_frames([12,12],false,false,1024);
        let plan=CoverageGpuPlan::new_with_frame_local_reuse(&p,1,2,2,1024,8192,102400,true).unwrap();
        assert!(plan.reuse_admitted());assert_eq!(plan.unique_clip_count(),1);
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
