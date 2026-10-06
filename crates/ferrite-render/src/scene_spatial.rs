//! Retained geographic area envelopes. Keep unsupported/invalid commands on
//! their original path; exact clipping and topology validation remain authoritative.
use crate::{AreaFillType, DrawingInstruction};
use ferrite_kernel::spatial_hierarchy::{SpatialBounds, SpatialHierarchy};
#[derive(Debug, Default)]
pub struct SceneSpatialIndex {
    pub areas: SpatialHierarchy<2>,
}
impl SceneSpatialIndex {
    pub fn compile(instructions: &[DrawingInstruction]) -> Self {
        let mut entries = Vec::new();
        for (id, item) in instructions.iter().enumerate() {
            if entries.len() >= 131072 {
                break;
            }
            let DrawingInstruction::Area(area) = item else {
                continue;
            };
            if !matches!(
                area.fill,
                AreaFillType::Solid(_)
                    | AreaFillType::Pattern { .. }
                    | AreaFillType::HatchFill { .. }
            ) || area.exterior.is_empty()
            {
                continue;
            }
            let mut b = SpatialBounds {
                min: [f64::INFINITY; 2],
                max: [f64::NEG_INFINITY; 2],
            };
            let mut valid = true;
            for p in area.exterior.iter().chain(area.interiors.iter().flatten()) {
                if !p.x.is_finite() || !p.y.is_finite() {
                    valid = false;
                    break;
                }
                b.min[0] = b.min[0].min(p.x);
                b.min[1] = b.min[1].min(p.y);
                b.max[0] = b.max[0].max(p.x);
                b.max[1] = b.max[1].max(p.y);
            }
            if valid && b.valid() {
                entries.push((id, b));
            }
        }
        Self {
            areas: SpatialHierarchy::build(entries).expect("validated area bounds"),
        }
    }
}
