//! Picking and feature reports: which features lie under a screen point of a
//! published scene, and plain-data descriptions of them.
//!
//! What was actually drawn (displayed symbol positions, line suppression,
//! coverage clipping, the selection index) belongs to the renderer, which
//! answers through [`PickSurface`]; the ordering and de-duplication of hits
//! and the feature lookup are engine logic. [`GeometryPicker`] is a
//! renderer-free surface that treats every instruction as displayed.
use crate::dataset_pick_fields;
use crate::state::EngineState;
use ferrite_render::{DrawingInstruction, LineSpan, RenderContext, ScreenPoint};

/// A pickable item: a drawn point symbol, or the nearest point of a drawn
/// line or area.
#[derive(Clone, Debug, PartialEq)]
pub struct PickCandidate {
    /// Instruction index in the published context (None for symbols whose
    /// instruction is not known).
    pub source: Option<usize>,
    pub plane: ferrite_kernel::CompositionPlane,
    /// Point = 0, line = 1, area = 2; geometry breaks ties within a display plane/priority.
    pub kind: u8,
    pub world_x: f64,
    pub world_y: f64,
    pub longitude_shift: f64,
    pub feature_id: i64,
    pub screen_x: f32,
    pub screen_y: f32,
    /// Drawing priority (higher = drawn on top, should be selected first)
    pub priority: i32,
    pub symbol_ref: String,
    /// Cell index this symbol belongs to (for correct feature lookup in multi-cell scenarios)
    pub cell_index: Option<u32>,
}

impl PickCandidate {
    /// Higher display planes first, then priority, geometry kind, distance,
    /// cell and feature.
    pub fn compare_hits(a: &(Self, f64), b: &(Self, f64)) -> std::cmp::Ordering {
        b.0.plane
            .cmp(&a.0.plane)
            .then(b.0.priority.cmp(&a.0.priority))
            .then(a.0.kind.cmp(&b.0.kind))
            .then(a.1.total_cmp(&b.1))
            .then(a.0.cell_index.cmp(&b.0.cell_index))
            .then(a.0.feature_id.cmp(&b.0.feature_id))
    }
}

/// What a renderer knows about the frame it displayed.
pub trait PickSurface {
    /// Whether the world is repeated east and west (passes 1 and 2).
    fn longitude_wrapping(&self) -> bool;
    /// Where a point symbol anchored at `anchor` was displayed in a wrap
    /// pass (0 centre, 1 west, 2 east); None when it was not displayed.
    fn displayed_symbol_screen(&self, anchor: [f32; 2], pass: usize) -> Option<[f32; 2]>;
    /// Whether instruction `index` is visible (not clipped by coverage) at
    /// `pixel` in a wrap pass.
    fn coverage_fragment_visible(&self, index: usize, pass: usize, pixel: [f32; 2]) -> bool;
    /// Line and area instructions near `query` (an index of the frame).
    fn selection_candidates(
        &self,
        context: &RenderContext,
        query: ScreenPoint,
        radius: f64,
    ) -> Vec<usize>;
    /// Every displayed line and area instruction.
    fn displayed_geometry(&self) -> &[usize];
    /// The displayed spans of a suppressed line (None: all of it).
    fn displayed_line_spans(&self, index: usize) -> Option<&[LineSpan]>;
    /// The point symbols drawn in the frame.
    fn point_symbols(&self) -> &[PickCandidate] {
        &[]
    }
}

/// Features near screen point (`x`, `y`) within `radius` pixels: the point
/// `symbols` and, with a surface, the lines and areas it displayed (from its
/// selection index when `indexed`, else every displayed geometry). Sorted
/// by display plane, priority, geometry, then distance; one hit per feature.
pub fn find_features_at(
    context: &RenderContext,
    symbols: &[PickCandidate],
    surface: Option<&dyn PickSurface>,
    x: f64,
    y: f64,
    radius: f32,
    indexed: bool,
) -> Vec<PickCandidate> {
    if !x.is_finite() || !y.is_finite() || !radius.is_finite() || radius < 0. {
        return Vec::new();
    }

    let wrapping = surface.is_some_and(|s| s.longitude_wrapping());
    let offsets = [0., -360., 360.];
    let mut nearby = Vec::new();
    for symbol in symbols {
        for &shift in &offsets[..if wrapping { 3 } else { 1 }] {
            let pass = if shift < 0. {
                1
            } else if shift > 0. {
                2
            } else {
                0
            };
            let screen = match surface {
                Some(surface) => {
                    match surface.displayed_symbol_screen([symbol.screen_x, symbol.screen_y], pass)
                    {
                        Some(screen) => screen,
                        None => continue,
                    }
                }
                None => [symbol.screen_x, symbol.screen_y],
            };
            let distance = (screen[0] - x as f32).hypot(screen[1] - y as f32);
            if distance <= radius {
                if let (Some(surface), Some(source)) = (surface, symbol.source) {
                    if !surface.coverage_fragment_visible(source, pass, [x as f32, y as f32]) {
                        continue;
                    }
                }
                let mut hit = symbol.clone();
                hit.screen_x = screen[0];
                hit.screen_y = screen[1];
                hit.longitude_shift = shift;
                nearby.push((hit, distance as f64));
            }
        }
    }
    if let Some(surface) = surface {
        let candidates: std::borrow::Cow<'_, [usize]> = if indexed {
            std::borrow::Cow::Owned(surface.selection_candidates(
                context,
                ScreenPoint::new(x as f32, y as f32),
                radius as f64 * 0.4,
            ))
        } else {
            std::borrow::Cow::Borrowed(surface.displayed_geometry())
        };
        for &index in candidates.iter() {
            let Some(instruction) = context.raw_instructions().get(index) else {
                continue;
            };
            let (kind, cell_index, feature_id) = match instruction {
                DrawingInstruction::Line(line) => (1, line.cell_index, line.feature_id),
                DrawingInstruction::Area(area) => (2, area.cell_index, area.feature_id),
                _ => continue,
            };
            let Some(feature_id) = feature_id else {
                continue;
            };
            if let Some(wrapped_hit) = ferrite_render::hit_geometry_wrapped_visible_in_context(
                context,
                index,
                ScreenPoint::new(x as f32, y as f32),
                radius as f64 * 0.4,
                surface.displayed_line_spans(index),
                wrapping,
            ) {
                let hit = wrapped_hit.hit;
                let coverage_pass = if wrapped_hit.longitude_shift < 0. {
                    1
                } else if wrapped_hit.longitude_shift > 0. {
                    2
                } else {
                    0
                };
                if !surface.coverage_fragment_visible(
                    index,
                    coverage_pass,
                    [hit.nearest.x, hit.nearest.y],
                ) {
                    continue;
                }
                let world = context.scaler.screen_to_world(hit.nearest);
                nearby.push((
                    PickCandidate {
                        source: Some(index),
                        plane: instruction
                            .display_plane()
                            .composition_plane(ferrite_kernel::CompositionStage::Chart),
                        kind,
                        cell_index,
                        feature_id,
                        world_x: world.x - wrapped_hit.longitude_shift,
                        world_y: world.y,
                        longitude_shift: wrapped_hit.longitude_shift,
                        screen_x: hit.nearest.x,
                        screen_y: hit.nearest.y,
                        priority: instruction.priority().0,
                        symbol_ref: String::new(),
                    },
                    hit.distance,
                ));
            }
        }
    }
    nearby.sort_by(PickCandidate::compare_hits);
    let mut seen = std::collections::HashSet::new();
    nearby
        .into_iter()
        .filter_map(|(hit, _)| seen.insert((hit.cell_index, hit.feature_id)).then_some(hit))
        .collect()
}

/// A surface without a renderer: every line and area instruction counts as
/// displayed, nothing is clipped, no wrapping. Point symbols are the point
/// instructions at their projected anchors ([`GeometryPicker::point_symbols`]).
pub struct GeometryPicker {
    geometry: Vec<usize>,
    symbols: Vec<PickCandidate>,
}

impl GeometryPicker {
    pub fn new(context: &RenderContext) -> Self {
        Self {
            symbols: Self::symbols_of(context),
            geometry: context
                .raw_instructions()
                .iter()
                .enumerate()
                .filter(|(_, i)| {
                    matches!(i, DrawingInstruction::Line(_) | DrawingInstruction::Area(_))
                })
                .map(|(index, _)| index)
                .collect(),
        }
    }
    /// The point instructions with a feature, as pick candidates.
    pub fn symbols_of(context: &RenderContext) -> Vec<PickCandidate> {
        context
            .raw_instructions()
            .iter()
            .enumerate()
            .filter_map(|(index, instruction)| {
                let DrawingInstruction::Point(point) = instruction else {
                    return None;
                };
                let feature_id = point.feature_id?;
                let screen = context.scaler.world_to_screen(point.position);
                Some(PickCandidate {
                    source: Some(index),
                    plane: instruction
                        .display_plane()
                        .composition_plane(ferrite_kernel::CompositionStage::Chart),
                    kind: 0,
                    world_x: point.position.x,
                    world_y: point.position.y,
                    longitude_shift: 0.,
                    feature_id,
                    screen_x: screen.x,
                    screen_y: screen.y,
                    priority: instruction.priority().0,
                    symbol_ref: point.symbol_ref.clone(),
                    cell_index: point.cell_index,
                })
            })
            .collect()
    }
}

impl PickSurface for GeometryPicker {
    fn longitude_wrapping(&self) -> bool {
        false
    }
    fn displayed_symbol_screen(&self, anchor: [f32; 2], _pass: usize) -> Option<[f32; 2]> {
        Some(anchor)
    }
    fn coverage_fragment_visible(&self, _index: usize, _pass: usize, _pixel: [f32; 2]) -> bool {
        true
    }
    fn selection_candidates(
        &self,
        _context: &RenderContext,
        _query: ScreenPoint,
        _radius: f64,
    ) -> Vec<usize> {
        self.geometry.clone()
    }
    fn displayed_geometry(&self) -> &[usize] {
        &self.geometry
    }
    fn displayed_line_spans(&self, _index: usize) -> Option<&[LineSpan]> {
        None
    }
    fn point_symbols(&self) -> &[PickCandidate] {
        &self.symbols
    }
}

/// Plain-data description of a picked ENC feature.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeatureReport {
    /// Feature type code (or the symbol name when the feature is unknown).
    pub feature_type: String,
    pub feature_id: i64,
    pub foid: Option<String>,
    pub cell_index: Option<u32>,
    pub primitive_type: String,
    /// Source dataset path.
    pub source: Option<String>,
    /// Attribute rows (name, value) resolved through the cell's own FC.
    pub attributes: Vec<(String, String)>,
    /// Feature Catalogue definition, when the type was resolved.
    pub definition: Option<String>,
}

impl EngineState {
    /// Describe ENC feature `feature_id` of loaded cell `cell_index` with the
    /// Feature Catalogue bound to that cell. An index-free hit cannot
    /// identify an ENC feature: only `fallback_type` is reported.
    pub fn describe_feature(
        &self,
        cell_index: Option<u32>,
        feature_id: i64,
        fallback_type: &str,
    ) -> FeatureReport {
        let feature = cell_index.and_then(|cell_idx| {
            self.cells
                .get(cell_idx as usize)
                .and_then(|cell| cell.features.get(&feature_id))
        });
        let feature_code = feature
            .and_then(|f| f.feature_code.as_deref())
            .unwrap_or(fallback_type)
            .to_string();
        let source_fc = cell_index
            .and_then(|index| self.catalogue_owner_for_cell(index as usize))
            .map(|owner| {
                let fc: &ferrite_feature_catalog::FeatureCatalogue = owner.fc();
                fc
            });
        let fields = feature
            .map(|f| dataset_pick_fields::resolve(source_fc, &feature_code, &f.attributes))
            .unwrap_or_default();
        let definition = fields.resolved.then_some(fields.definition).flatten();
        FeatureReport {
            feature_type: feature_code,
            feature_id,
            foid: feature
                .and_then(|f| f.foid.as_ref())
                .map(ToString::to_string),
            cell_index,
            primitive_type: feature
                .map(|f| format!("{:?}", f.primitive_type))
                .unwrap_or_else(|| "Unknown".into()),
            source: cell_index
                .and_then(|i| self.cells.get(i as usize))
                .map(|c| c.file_path.display().to_string()),
            attributes: fields.attributes,
            definition,
        }
    }
}
