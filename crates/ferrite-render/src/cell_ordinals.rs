//! Per-cell ordinal lists in the exact sorted instruction order, with each
//! cell's geographic bounds. A scene build can then visit only the cells whose
//! bounds meet the view and merge their (ascending) ordinals, preserving the
//! original drawing order. Anything whose drawn extent is not bounded by its
//! source geometry is listed in `always` and visited unconditionally.
use crate::DrawingInstruction;

#[derive(Debug, Default)]
pub struct CellOrdinals {
    /// (min_x, min_y, max_x, max_y) in source longitude/latitude.
    pub bounds: (f64, f64, f64, f64),
    pub ordinals: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct CellOrdinalIndex {
    pub cells: Vec<CellOrdinals>,
    pub always: Vec<u32>,
    pub total: usize,
}

/// Source-geometry bounds when they bound everything the instruction draws.
fn bounded_extent(instruction: &DrawingInstruction) -> Option<(f64, f64, f64, f64)> {
    let origin = instruction.portrayal_origin();
    if origin.requires_view_reprojection() || origin.is_device_fixed() {
        return None;
    }
    let points: &[crate::WorldPoint] = match instruction {
        DrawingInstruction::Point(p) => {
            if p.line_placement.is_some() || p.dependency.is_some() {
                return None;
            }
            std::slice::from_ref(&p.position)
        }
        DrawingInstruction::Text(t) => {
            if t.dependency.is_some() {
                return None;
            }
            std::slice::from_ref(&t.position)
        }
        DrawingInstruction::Line(l) => {
            if l.screen_ray.is_some() || l.portrayal_path.is_some() || l.dependency.is_some() {
                return None;
            }
            &l.points
        }
        DrawingInstruction::Area(a) => {
            if a.dependency.is_some() {
                return None;
            }
            &a.exterior
        }
    };
    let mut b = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for p in points {
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        b = (b.0.min(p.x), b.1.min(p.y), b.2.max(p.x), b.3.max(p.y));
    }
    (b.0 <= b.2 && b.1 <= b.3).then_some(b)
}

impl CellOrdinalIndex {
    pub fn compile(instructions: &[DrawingInstruction]) -> Self {
        let mut cells: Vec<CellOrdinals> = Vec::new();
        let mut always = Vec::new();
        if u32::try_from(instructions.len()).is_err() {
            return Self {
                cells,
                always: Vec::new(),
                total: 0,
            };
        }
        for (ordinal, instruction) in instructions.iter().enumerate() {
            let ordinal = ordinal as u32;
            let (Some(cell), Some(b)) = (instruction.cell_index(), bounded_extent(instruction))
            else {
                always.push(ordinal);
                continue;
            };
            let cell = cell as usize;
            if cells.len() <= cell {
                cells.resize_with(cell + 1, || CellOrdinals {
                    bounds: (
                        f64::INFINITY,
                        f64::INFINITY,
                        f64::NEG_INFINITY,
                        f64::NEG_INFINITY,
                    ),
                    ordinals: Vec::new(),
                });
            }
            let entry = &mut cells[cell];
            entry.bounds = (
                entry.bounds.0.min(b.0),
                entry.bounds.1.min(b.1),
                entry.bounds.2.max(b.2),
                entry.bounds.3.max(b.3),
            );
            entry.ordinals.push(ordinal);
        }
        for cell in &mut cells {
            cell.ordinals.shrink_to_fit();
        }
        always.shrink_to_fit();
        Self {
            cells,
            always,
            total: instructions.len(),
        }
    }

    /// Ascending ordinals of `always` plus every cell accepted by `visible`;
    /// None when nearly everything is visible (the full range is cheaper).
    pub fn visible_ordinals(
        &self,
        mut visible: impl FnMut((f64, f64, f64, f64)) -> bool,
    ) -> Option<Vec<u32>> {
        if self.total == 0 {
            return None;
        }
        let chosen: Vec<&CellOrdinals> = self
            .cells
            .iter()
            .filter(|c| !c.ordinals.is_empty() && visible(c.bounds))
            .collect();
        let count = self.always.len() + chosen.iter().map(|c| c.ordinals.len()).sum::<usize>();
        if count * 4 > self.total * 3 {
            return None;
        }
        let mut out = Vec::with_capacity(count);
        out.extend_from_slice(&self.always);
        for cell in chosen {
            out.extend_from_slice(&cell.ordinals);
        }
        out.sort_unstable();
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AreaInstruction, LineInstruction, PointInstruction, WorldPoint};
    fn point(x: f64, cell: usize) -> DrawingInstruction {
        DrawingInstruction::Point(
            PointInstruction::new("S".into(), WorldPoint::new(x, 0.)).with_cell_index(cell),
        )
    }
    #[test]
    fn visible_cells_keep_global_order_and_unbounded_items_are_always_kept() {
        let mut items = vec![
            point(0., 0),
            point(10., 1),
            point(0.5, 0),
            DrawingInstruction::Line(
                LineInstruction::new(vec![WorldPoint::new(10., 0.), WorldPoint::new(11., 1.)])
                    .with_cell_index(1),
            ),
            DrawingInstruction::Point(PointInstruction::new("S".into(), WorldPoint::new(10., 0.))),
            DrawingInstruction::Area(Box::new(
                AreaInstruction::new(vec![
                    WorldPoint::new(0., 0.),
                    WorldPoint::new(1., 0.),
                    WorldPoint::new(1., 1.),
                ])
                .with_cell_index(0),
            )),
        ];
        // An augmented (view-dependent) point in an offscreen cell stays.
        let mut local =
            PointInstruction::new("S".into(), WorldPoint::new(10., 0.)).with_cell_index(1);
        local.portrayal_origin =
            crate::PortrayalOrigin::augmented_local_point(WorldPoint::new(10., 0.), [5., 5.])
                .unwrap();
        items.push(DrawingInstruction::Point(local));
        let index = CellOrdinalIndex::compile(&items);
        assert_eq!(index.cells[0].ordinals, vec![0, 2, 5]);
        assert_eq!(index.cells[1].ordinals, vec![1, 3]);
        assert_eq!(index.always, vec![4, 6]);
        let only_cell_0 = index.visible_ordinals(|b| b.0 < 5.).expect("small subset");
        assert_eq!(only_cell_0, vec![0, 2, 4, 5, 6]);
        assert!(index.visible_ordinals(|_| true).is_none());
    }
}
