//! Instruction-order coverage decisions. Each view or longitude copy prepares
//! its own pass with the actual source projection. Geometry changes invalidate
//! the binding rather than reusing a visibility vector for different commands.
use crate::error::Result;
use crate::{DrawingInstruction, PointOriginGeometry, PortrayalOrigin, RenderError};
use ferrite_kernel::coverage_frame::{CoverageFrame, CoverageSource, FrameCoverageDecision};
use ferrite_kernel::coverage_rendering::InstructionOrigin;
use std::sync::Arc;
#[derive(Debug, Clone, Copy)]
pub enum InstructionCoverageClass {
    Exempt,
    Dataset(usize),
}
#[derive(Debug)]
pub struct PreparedCoveragePass {
    frame: Arc<CoverageFrame>,
    decisions: Vec<FrameCoverageDecision>,
}
impl PreparedCoveragePass {
    pub fn prepare(
        instructions: &[DrawingInstruction],
        frame: Arc<CoverageFrame>,
        mut classify: impl FnMut(&DrawingInstruction) -> Result<InstructionCoverageClass>,
        mut project: impl FnMut(&DrawingInstruction, &PointOriginGeometry) -> Result<Option<[f64; 2]>>,
    ) -> Result<Self> {
        let mut decisions = Vec::with_capacity(instructions.len());
        for instruction in instructions {
            let decision = match classify(instruction)? {
                InstructionCoverageClass::Exempt => FrameCoverageDecision::Unclipped,
                InstructionCoverageClass::Dataset(dataset_id) => {
                    let origin = match instruction.portrayal_origin() {
                        PortrayalOrigin::CoverageExempt => {
                            return Err(RenderError::Render(
                                "Host overlay cannot be assigned a product dataset".into(),
                            ))
                        }
                        PortrayalOrigin::Unspecified => {
                            return Err(RenderError::Render(
                                "Missing portrayal origin for coverage binding".into(),
                            ))
                        }
                        PortrayalOrigin::NonPoint => InstructionOrigin::NonPoint,
                        PortrayalOrigin::Point(point) => match project(instruction, point)? {
                            Some(p) => InstructionOrigin::Point(p),
                            None => {
                                // Validate identity before applying the source projection.
                                frame
                                    .decision(CoverageSource::Dataset {
                                        dataset_id,
                                        origin: InstructionOrigin::NonPoint,
                                    })
                                    .map_err(|e| RenderError::Render(e.to_string()))?;
                                decisions.push(FrameCoverageDecision::Hidden);
                                continue;
                            }
                        },
                    };
                    frame
                        .decision(CoverageSource::Dataset { dataset_id, origin })
                        .map_err(|e| RenderError::Render(e.to_string()))?
                }
            };
            decisions.push(decision);
        }
        Ok(Self { frame, decisions })
    }
    /// Resolve each immutable source once in this actual frame, then expand in
    /// original instruction order. An absent projected copy still validates
    /// its dataset; no hidden source can bypass the namespace check.
    pub fn prepare_source_slots(
        frame:Arc<CoverageFrame>,sources:&[(CoverageSource,bool)],slots:&[usize],
    )->Result<Self> {
        if sources.len()>slots.len() || slots.iter().any(|i|*i>=sources.len()) {
            return Err(RenderError::Render("Coverage source slot outside binding".into()));
        }
        let resolved=sources.iter().map(|(source,absent)| {
            let decision=frame.decision(*source).map_err(|e|RenderError::Render(e.to_string()))?;
            Ok(if *absent {FrameCoverageDecision::Hidden} else {decision})
        }).collect::<Result<Vec<_>>>()?;
        let decisions=slots.iter().map(|i|resolved[*i]).collect();
        Ok(Self {frame,decisions})
    }
    pub fn decision(&self, index: usize) -> Result<FrameCoverageDecision> {
        self.decisions
            .get(index)
            .copied()
            .ok_or_else(|| RenderError::Render("Coverage instruction outside binding".into()))
    }
    pub fn frame(&self) -> &CoverageFrame {
        &self.frame
    }
    pub fn accepts_fragment(&self, index: usize, point: [f64; 2]) -> Result<bool> {
        self.frame
            .accepts_fragment(self.decision(index)?, point)
            .map_err(|e| RenderError::Render(e.to_string()))
    }
}
#[derive(Debug)]
pub struct PreparedCoverage {
    revision: u64,
    view_revision: u64,
    count: usize,
    passes: Vec<PreparedCoveragePass>,
}
impl PreparedCoverage {
    pub fn new(
        revision: u64,
        view_revision: u64,
        count: usize,
        passes: Vec<PreparedCoveragePass>,
    ) -> Result<Self> {
        if passes.is_empty() || passes.iter().any(|p| p.decisions.len() != count) {
            return Err(RenderError::Render(
                "Missing or mismatched coverage projection passes".into(),
            ));
        }
        Ok(Self {
            revision,
            view_revision,
            count,
            passes,
        })
    }
    pub fn validate(&self, revision: u64, view_revision: u64, count: usize) -> Result<()> {
        if self.revision != revision || self.view_revision != view_revision || self.count != count {
            return Err(RenderError::Render(
                "Coverage binding belongs to stale instruction geometry".into(),
            ));
        }
        Ok(())
    }
    /// A command can execute when any projected copy is visible. Each draw pass
    /// still uses its own decision; the union is only the execution pre-filter.
    pub fn visibility(&self, revision: u64, view_revision: u64, count: usize) -> Result<Vec<bool>> {
        self.validate(revision, view_revision, count)?;
        Ok((0..count)
            .map(|i| {
                self.passes
                    .iter()
                    .any(|p| p.decisions[i] != FrameCoverageDecision::Hidden)
            })
            .collect())
    }
    /// Fuse the immutable coverage union into the caller's calendar mask.
    /// Validate all binding dimensions before touching the destination, matching
    /// `visibility` followed by the caller's elementwise AND without its Vec.
    pub fn intersect_visibility(&self, revision: u64, view_revision: u64, visible: &mut [bool]) -> Result<()> {
        self.validate(revision, view_revision, visible.len())?;
        for (i, value) in visible.iter_mut().enumerate() {
            *value &= self.passes.iter().any(|p| p.decisions[i] != FrameCoverageDecision::Hidden);
        }
        Ok(())
    }
    pub fn pass_count(&self) -> usize {
        self.passes.len()
    }
    /// Batching may merge only commands with the same decision in every copy.
    pub fn same_draw_decisions(&self, first: usize, second: usize) -> Result<bool> {
        for pass in &self.passes {
            if pass.decision(first)? != pass.decision(second)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn pass(&self, index: usize) -> Result<&PreparedCoveragePass> {
        self.passes
            .get(index)
            .ok_or_else(|| RenderError::Render("Coverage projection pass outside binding".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PointInstruction, WorldPoint};
    use ferrite_kernel::coverage_selection::{
        CoverageFootprint, Region, SelectedCoverage, Selection,
    };
    use ferrite_kernel::scale_policy::CoverageScaleRange;
    fn frame() -> Arc<CoverageFrame> {
        let region =
            Region::from_rings(&[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]], &[])
                .unwrap();
        let footprints = [180000, 45000]
            .into_iter()
            .enumerate()
            .map(|(id, min)| CoverageFootprint {
                dataset_id: id,
                coverage_id: id as i64,
                region: region.clone(),
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
                .map(|index| SelectedCoverage {
                    inventory_index: index,
                    selection_band: 10,
                    selected_to_fill_gap: false,
                })
                .collect(),
            uncovered: Region::from_polygons(vec![]).unwrap(),
        };
        Arc::new(CoverageFrame::new(&footprints, &selection, &region, [12, 12], 4096).unwrap())
    }
    fn instructions() -> Vec<DrawingInstruction> {
        let mut a = DrawingInstruction::Point(PointInstruction::new(
            "X".into(),
            WorldPoint::new(500., 500.),
        ));
        a.set_portrayal_origin(PortrayalOrigin::feature_point(WorldPoint::new(1., 1.)).unwrap());
        let mut b = DrawingInstruction::Point(PointInstruction::new(
            "Y".into(),
            WorldPoint::new(500., 500.),
        ));
        b.set_portrayal_origin(PortrayalOrigin::NonPoint);
        vec![a, b]
    }
    #[test]
    fn source_slots_validate_absent_identity_and_preserve_order() {
        let invalid=CoverageSource::Dataset {dataset_id:99,origin:InstructionOrigin::NonPoint};
        assert!(PreparedCoveragePass::prepare_source_slots(frame(),&[(invalid,true)],&[0]).is_err());
        assert!(PreparedCoveragePass::prepare_source_slots(frame(),&[(CoverageSource::Exempt,false)],&[1]).is_err());
        let nonpoint=CoverageSource::Dataset {dataset_id:0,origin:InstructionOrigin::NonPoint};
        let pass=PreparedCoveragePass::prepare_source_slots(frame(),&[(nonpoint,false),(CoverageSource::Exempt,false)],&[1,0,1]).unwrap();
        assert_eq!(pass.decision(0).unwrap(),FrameCoverageDecision::Unclipped);
        assert_eq!(pass.decision(1).unwrap(),FrameCoverageDecision::ClipDataset(0));
        assert_eq!(pass.decision(2).unwrap(),FrameCoverageDecision::Unclipped);
        let absent=PreparedCoveragePass::prepare_source_slots(frame(),&[(nonpoint,true)],&[0,0]).unwrap();
        assert_eq!(absent.decision(0).unwrap(),FrameCoverageDecision::Hidden);
        assert_eq!(absent.decision(1).unwrap(),FrameCoverageDecision::Hidden);
    }
    #[test]
    fn projected_copies_use_source_geometry_and_share_fragment_decisions() {
        let instructions = instructions();
        let a = PreparedCoveragePass::prepare(
            &instructions,
            frame(),
            |_| Ok(InstructionCoverageClass::Dataset(0)),
            |instruction, source| {
                assert!(matches!(instruction, DrawingInstruction::Point(_)));
                assert_eq!(
                    source,
                    &PointOriginGeometry::FeaturePoint(WorldPoint::new(1., 1.))
                );
                Ok(Some([1., 1.]))
            },
        )
        .unwrap();
        assert_eq!(a.decision(0).unwrap(), FrameCoverageDecision::Hidden);
        assert_eq!(
            a.decision(1).unwrap(),
            FrameCoverageDecision::ClipDataset(0)
        );
        assert!(!a.accepts_fragment(1, [1.5, 1.5]).unwrap());
        let b = PreparedCoveragePass::prepare(
            &instructions,
            frame(),
            |_| Ok(InstructionCoverageClass::Dataset(0)),
            |_, _| Ok(Some([11., 1.])),
        )
        .unwrap();
        assert_eq!(b.decision(0).unwrap(), FrameCoverageDecision::Unclipped);
        let prepared = PreparedCoverage::new(7, 8, 2, vec![a, b]).unwrap();
        assert_eq!(prepared.visibility(7, 8, 2).unwrap(), vec![true, true]);
        // Equal execution visibility does not permit mixing these draw ranges:
        // one point copy is hidden/unclipped, the other command needs a mask.
        assert!(!prepared.same_draw_decisions(0, 1).unwrap());
        assert!(prepared.same_draw_decisions(0, 2).is_err());
        assert!(!prepared
            .pass(0)
            .unwrap()
            .accepts_fragment(0, [1., 1.])
            .unwrap());
        assert!(prepared
            .pass(1)
            .unwrap()
            .accepts_fragment(0, [1., 1.])
            .unwrap());
        assert!(prepared.visibility(9, 8, 2).is_err());
        assert!(prepared.visibility(7, 9, 2).is_err());
    }
    #[test]
    fn missing_origin_invalid_source_and_stale_binding_are_errors() {
        let unspecified = [DrawingInstruction::Point(PointInstruction::new(
            "X".into(),
            WorldPoint::new(1., 1.),
        ))];
        assert!(PreparedCoveragePass::prepare(
            &unspecified,
            frame(),
            |_| Ok(InstructionCoverageClass::Dataset(0)),
            |_, _| Ok(None)
        )
        .is_err());
        let instructions = instructions();
        assert!(PreparedCoveragePass::prepare(
            &instructions,
            frame(),
            |_| Ok(InstructionCoverageClass::Dataset(99)),
            |_, _| Ok(None)
        )
        .is_err());
        let pass = PreparedCoveragePass::prepare(
            &instructions,
            frame(),
            |_| Ok(InstructionCoverageClass::Dataset(0)),
            |_, _| Ok(None),
        )
        .unwrap();
        assert_eq!(pass.decision(0).unwrap(), FrameCoverageDecision::Hidden);
        assert!(PreparedCoverage::new(7, 8, 1, vec![pass]).is_err());
        assert!(PreparedCoverage::new(7, 8, 2, vec![]).is_err());
    }
    #[test]
    fn fused_visibility_matches_independent_union_and_calendar_truth_table() {
        use FrameCoverageDecision::{Hidden, Unclipped, ClipDataset};
        for bits in 0u32..8 {
            let coverage=PreparedCoverage::new(7,8,3,vec![
                PreparedCoveragePass {frame:frame(),decisions:vec![Hidden,Unclipped,Hidden]},
                PreparedCoveragePass {frame:frame(),decisions:vec![Hidden,Hidden,ClipDataset(0)]},
                PreparedCoveragePass {frame:frame(),decisions:vec![Hidden,Hidden,Hidden]},
            ]).unwrap();
            let original=(0..3).map(|i| bits & (1<<i) !=0).collect::<Vec<_>>();
            let mut fused=original.clone();coverage.intersect_visibility(7,8,&mut fused).unwrap();
            assert_eq!(fused,vec![false,original[1],original[2]]);
            let mut reference=original;let union=coverage.visibility(7,8,3).unwrap();
            for (v,c) in reference.iter_mut().zip(union) {*v &=c;}
            assert_eq!(fused,reference);
        }
    }
    #[test]
    fn fused_stale_bindings_reject_before_any_destination_write() {
        let p=PreparedCoverage::new(7,8,3,vec![PreparedCoveragePass{frame:frame(),decisions:vec![FrameCoverageDecision::Hidden;3]}]).unwrap();
        for (rev,view,len) in [(9,8,3),(7,9,3),(7,8,2),(7,8,4)] {
            let mut mask=vec![true;len];let before=mask.clone();
            let original=p.visibility(rev,view,len).unwrap_err().to_string();
            assert_eq!(p.intersect_visibility(rev,view,&mut mask).unwrap_err().to_string(),original);
            assert_eq!(mask,before);
        }
    }
    #[test]
    fn fused_empty_binding_and_replaced_decisions_have_no_cached_stale_union() {
        let empty=PreparedCoverage::new(1,2,0,vec![PreparedCoveragePass{frame:frame(),decisions:vec![]}]).unwrap();
        empty.intersect_visibility(1,2,&mut []).unwrap();
        for hidden in [false,true,false] {
            let c=PreparedCoverage::new(7,8,1,vec![PreparedCoveragePass{frame:frame(),decisions:vec![if hidden {FrameCoverageDecision::Hidden}else{FrameCoverageDecision::Unclipped}]}]).unwrap();
            let mut mask=[true];c.intersect_visibility(7,8,&mut mask).unwrap();assert_eq!(mask,[!hidden]);
        }
    }

}
