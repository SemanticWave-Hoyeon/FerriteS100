//! FC-typed S-101 attribute paths and atomic portrayal composition plans.
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::{AttributeValueType, FeatureCatalogue};
use ferrite_interoperability::{Assignment, Catalogue, Decimal, Scalar, Selection};
use ferrite_render::{DisplayPlane, DrawingInstruction, GeometryType};
use ferrite_s100_core::{Attribute, FeatureRecord, S101Cell, SpatialPrimitiveType};
use std::collections::HashMap;
struct AttributeForest<'a> {
    attributes: &'a [Attribute],
    children: HashMap<(usize, &'a str), Vec<usize>>,
}
impl<'a> AttributeForest<'a> {
    fn new(attributes: &'a [Attribute]) -> Result<Self> {
        let mut children: HashMap<(usize, &str), Vec<usize>> = HashMap::new();
        for (i, a) in attributes.iter().enumerate() {
            let parent = a.paix as usize;
            ensure!(
                parent <= attributes.len() && parent != i + 1,
                "Invalid S-101 attribute parent at {}",
                i + 1
            );
            children
                .entry((
                    parent,
                    a.code.as_deref().context("Unmapped attribute code")?,
                ))
                .or_default()
                .push(i + 1);
        }
        // Each parent chain is visited once. PAIX is a vector position, not ATIX.
        let mut colors = vec![0u8; attributes.len() + 1];
        colors[0] = 2;
        for start in 1..=attributes.len() {
            let mut chain = Vec::new();
            let mut current = start;
            while colors[current] == 0 {
                colors[current] = 1;
                chain.push(current);
                current = attributes[current - 1].paix as usize;
            }
            ensure!(colors[current] == 2, "Cyclic S-101 attribute hierarchy");
            for i in chain {
                colors[i] = 2;
            }
        }
        Ok(Self {
            attributes,
            children,
        })
    }
    fn scalar(&self, path: &[String], fc: &FeatureCatalogue) -> Result<Option<Scalar>> {
        let code = path.last().context("Empty attribute path")?;
        let definition = fc
            .simple_attributes
            .get(code)
            .context("Filter leaf is not a simple FC attribute")?;
        for segment in &path[..path.len() - 1] {
            ensure!(
                fc.complex_attributes.contains_key(segment),
                "Filter parent is not a complex FC attribute: {segment}"
            );
        }
        for pair in path.windows(2) {
            ensure!(
                fc.complex_attributes[&pair[0]]
                    .sub_attributes
                    .iter()
                    .any(|b| b.attribute_code == pair[1]),
                "Path is not bound by FC: {}/{}",
                pair[0],
                pair[1]
            );
        }
        let mut parents = vec![0usize];
        for segment in path {
            parents = parents
                .iter()
                .flat_map(|p| {
                    self.children
                        .get(&(*p, segment.as_str()))
                        .into_iter()
                        .flatten()
                        .copied()
                })
                .collect();
            if parents.is_empty() {
                return Ok(None);
            }
        }
        ensure!(
            parents.len() == 1,
            "Repeated attribute path requires a multiplicity policy: {}",
            path.join("/")
        );
        let raw = &self.attributes[parents[0] - 1].atvl;
        // S-100 10a-5.1.3: a present code with empty ATVL means unknown, including text.
        if raw.is_empty() {
            return Ok(None);
        }
        Ok(Some(match definition.value_type {
            AttributeValueType::Integer => {
                let digits = raw.strip_prefix('-').unwrap_or(raw);
                ensure!(
                    !digits.is_empty()
                        && digits.bytes().all(|c| c.is_ascii_digit())
                        && (digits == "0" || !digits.starts_with('0')),
                    "Invalid arbitrary-length integer attribute"
                );
                Scalar::Number(Decimal::parse(raw)?)
            }
            AttributeValueType::Real => Scalar::Number(Decimal::parse_real(raw)?),
            AttributeValueType::Enumeration => {
                let code = raw.parse::<u32>().context("Invalid enumeration code")?;
                ensure!(
                    definition.get_listed_value(code).is_some(),
                    "Enumeration code {code} is not in FC"
                );
                Scalar::Number(Decimal::parse(raw)?)
            }
            AttributeValueType::Boolean => {
                ensure!(raw == "0" || raw == "1", "Invalid S-101 boolean");
                Scalar::Number(Decimal::parse(raw)?)
            }
            _ => Scalar::Text(raw.clone()),
        }))
    }
}
fn feature_geometry(f: &FeatureRecord) -> &'static str {
    match f.primitive_type {
        SpatialPrimitiveType::Point => "point",
        SpatialPrimitiveType::MultiPoint => "pointSet",
        SpatialPrimitiveType::Curve | SpatialPrimitiveType::CompositeCurve => "curve",
        SpatialPrimitiveType::Surface => "surface",
        SpatialPrimitiveType::NoGeometry => "noGeometry",
    }
}
fn drawing_geometry(i: &DrawingInstruction) -> &'static str {
    match i {
        DrawingInstruction::Point(_) | DrawingInstruction::Text(_) => "point",
        DrawingInstruction::Line(_) => "curve",
        DrawingInstruction::Area(_) => "surface",
    }
}
struct Change {
    index: usize,
    cell: u32,
    feature: i64,
    geometry: GeometryType,
    /// None removes the instruction (Level 2 feature-type suppression).
    assignment: Option<Assignment>,
    /// Resolved substituteSymbolization for this instruction's kind.
    substitute: Option<Substitute>,
}
/// substituteSymbolization resolved against the owning portrayal catalogue.
#[derive(Clone)]
enum Substitute {
    Symbol {
        reference: String,
        rotation: f32,
        rotation_crs: ferrite_render::RotationCrs,
        scale: f32,
    },
    LineStyle {
        reference: String,
        style: Box<ferrite_render::LineStyle>,
    },
    /// A template area carrying only the substituted fill and fill_ref.
    AreaFill(Box<ferrite_render::AreaInstruction>),
    Text(Box<TextSubstitute>),
}
#[derive(Clone)]
struct TextSubstitute {
    text: String,
    size: f32,
    color: (ferrite_render::Color, String, f32),
    background: Option<(ferrite_render::Color, String, f32)>,
    h_align: ferrite_render::HAlign,
    v_align: ferrite_render::VAlign,
    rotation: f32,
}
/// A colour token of the active profile, with Part 9 transparency.
fn substitute_color(
    pc: &ferrite_portrayal_catalog::PortrayalCatalogue,
    profile: &str,
    color: &ferrite_interoperability::ColorSubstitute,
) -> Result<(ferrite_render::Color, String, f32)> {
    ensure!(
        pc.get_color(profile, &color.token).is_some(),
        "Substitute colour {} is not in profile {profile}",
        color.token
    );
    Ok((
        crate::lookup_pc_color(pc, &color.token, profile),
        color.token.clone(),
        1. - color.transparency() as f32,
    ))
}
/// Part 9 Symbol defaults apply to omitted attributes: the substitute
/// replaces the whole symbol element, not individual attributes.
fn resolve_substitute(
    substitution: &ferrite_interoperability::Substitution,
    instruction: &DrawingInstruction,
    portrayal: Option<(&ferrite_portrayal_catalog::PortrayalCatalogue, &str)>,
) -> Result<Option<Substitute>> {
    match instruction {
        DrawingInstruction::Point(_) => {
            let Some(symbol) = &substitution.symbol else {
                return Ok(None);
            };
            let (pc, _) =
                portrayal.context("Symbol substitution requires the portrayal catalogue")?;
            ensure!(
                pc.get_symbol(&symbol.reference).is_some(),
                "Substitute symbol {} is not in the portrayal catalogue",
                symbol.reference
            );
            Ok(Some(Substitute::Symbol {
                reference: symbol.reference.clone(),
                rotation: symbol.rotation().unwrap_or(0.) as f32,
                rotation_crs: ferrite_render::RotationCrs::from_lua(
                    symbol.rotation_crs.as_deref().unwrap_or("PortrayalCRS"),
                )
                .map_err(anyhow::Error::msg)?,
                scale: symbol.scale_factor().unwrap_or(1.) as f32,
            }))
        }
        DrawingInstruction::Line(_) => {
            let Some(reference) = &substitution.line_style else {
                return Ok(None);
            };
            let (pc, profile) =
                portrayal.context("Line style substitution requires the portrayal catalogue")?;
            let mut styles =
                crate::resolve_line_strokes(std::slice::from_ref(reference), None, pc, profile)
                    .with_context(|| format!("Substitute line style {reference}"))?;
            Ok(Some(Substitute::LineStyle {
                reference: reference.clone(),
                style: Box::new(styles.remove(0)),
            }))
        }
        DrawingInstruction::Area(_) => {
            use ferrite_interoperability::AreaFillSubstitute;
            use ferrite_portrayal_catalog::AreaFillType;
            let Some(fill) = &substitution.area_fill else {
                return Ok(None);
            };
            let (pc, profile) =
                portrayal.context("Area fill substitution requires the portrayal catalogue")?;
            let lookup = |token: &str| -> Result<ferrite_render::Color> {
                ensure!(
                    pc.get_color(profile, token).is_some(),
                    "Substitute colour {token} is not in profile {profile}"
                );
                Ok(crate::lookup_pc_color(pc, token, profile))
            };
            let template = ferrite_render::AreaInstruction::new(Vec::new());
            let area = match fill {
                AreaFillSubstitute::Color(color) => {
                    let (c, token, opacity) = substitute_color(pc, profile, color)?;
                    let mut area = template.with_solid_fill_token_opacity(c, &token, opacity);
                    area.fill_ref = None;
                    area
                }
                AreaFillSubstitute::Reference(reference) => {
                    let fill = pc.area_fills.get(reference.as_str()).with_context(|| {
                        format!("Substitute area fill {reference} is not in the PC")
                    })?;
                    let mut area = match &fill.fill_type {
                        AreaFillType::Color(c) => {
                            template.with_solid_fill_token(lookup(&c.color_token)?, &c.color_token)
                        }
                        AreaFillType::Hatch(h) => template.with_hatch_fill_token(
                            lookup(&h.line_color)?,
                            &h.line_color,
                            h.line_width as f32,
                            h.spacing as f32,
                            h.angle as f32,
                        ),
                        AreaFillType::Symbol(f) => template.with_pattern_fill(
                            f.symbol_ref.clone(),
                            (f.v1.x as f32, f.v1.y as f32),
                            (f.v2.x as f32, f.v2.y as f32),
                        ),
                        AreaFillType::Pattern(f) => template.with_pattern_fill(
                            f.symbol_ref.clone(),
                            (f.spacing_x as f32, 0.0),
                            (0.0, f.spacing_y as f32),
                        ),
                        AreaFillType::Pixmap(_) => {
                            anyhow::bail!("Substitute pixmap fill {reference} is not renderable")
                        }
                    };
                    area.fill_ref = Some(reference.clone());
                    area
                }
            };
            Ok(Some(Substitute::AreaFill(Box::new(area))))
        }
        DrawingInstruction::Text(_) => {
            let Some(text) = &substitution.text else {
                return Ok(None);
            };
            let (pc, profile) =
                portrayal.context("Text substitution requires the portrayal catalogue")?;
            Ok(Some(Substitute::Text(Box::new(TextSubstitute {
                text: text.text.clone(),
                size: text.body_size() as f32,
                color: substitute_color(pc, profile, &text.foreground)?,
                background: text
                    .background
                    .as_ref()
                    .map(|b| substitute_color(pc, profile, b))
                    .transpose()?,
                h_align: match text.horizontal_alignment.as_str() {
                    "Center" => ferrite_render::HAlign::Center,
                    "End" => ferrite_render::HAlign::Right,
                    _ => ferrite_render::HAlign::Left,
                },
                v_align: match text.vertical_alignment.as_str() {
                    "Top" => ferrite_render::VAlign::Top,
                    "Center" => ferrite_render::VAlign::Middle,
                    _ => ferrite_render::VAlign::Bottom,
                },
                rotation: text.rotation() as f32,
            }))))
        }
    }
}
/// Geographic extent [west, south, east, north] of a feature's source geometry.
/// None when any spatial reference is unresolved or there is no geometry, so
/// an incomplete feature is never suppressed.
fn feature_extent(cell: &S101Cell, f: &FeatureRecord) -> Option<[f64; 4]> {
    let mut extent: Option<[f64; 4]> = None;
    let mut add = |c: &ferrite_s100_core::Coordinate| {
        let e = extent.get_or_insert([c.x, c.y, c.x, c.y]);
        e[0] = e[0].min(c.x);
        e[1] = e[1].min(c.y);
        e[2] = e[2].max(c.x);
        e[3] = e[3].max(c.y);
    };
    let curve = |key, add: &mut dyn FnMut(&ferrite_s100_core::Coordinate)| -> bool {
        if let Some(c) = cell.curves.get(&key) {
            c.positions_iter().for_each(&mut *add);
            return true;
        }
        let Some(composite) = cell.composite_curves.get(&key) else {
            return false;
        };
        composite.curves.iter().all(|sub| {
            cell.curves
                .get(&sub.curve_id.key())
                .map(|c| c.positions_iter().for_each(&mut *add))
                .is_some()
        })
    };
    for spas in &f.spatial_associations {
        let key = spas.spatial_id.key();
        let resolved = if let Some(p) = cell.points.get(&key) {
            add(&p.position);
            true
        } else if let Some(m) = cell.multi_points.get(&key) {
            m.positions.iter().for_each(&mut add);
            true
        } else if let Some(surface) = cell.surfaces.get(&key) {
            // The exterior ring bounds the surface.
            surface
                .exterior_ring
                .iter()
                .all(|r| curve(r.curve_id.key(), &mut add))
        } else {
            curve(key, &mut add)
        };
        if !resolved {
            return None;
        }
    }
    extent.filter(|e| e.iter().all(|v| v.is_finite()))
}
pub struct InteroperabilityPlan {
    changes: Vec<Change>,
}
impl InteroperabilityPlan {
    pub fn len(&self) -> usize {
        self.changes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
    /// Validate every target first. A stale or invalid plan changes no instructions.
    pub fn apply(&self, instructions: &mut Vec<DrawingInstruction>) -> Result<()> {
        for c in &self.changes {
            let i = instructions
                .get(c.index)
                .context("Composition plan is stale")?;
            ensure!(
                i.cell_index() == Some(c.cell)
                    && i.feature_id() == Some(c.feature)
                    && i.geometry_type() == c.geometry,
                "Composition plan identity mismatch"
            );
            if let Some(a) = &c.assignment {
                ensure!(
                    a.plane.stage == ferrite_kernel::CompositionStage::Chart && a.priority >= 0,
                    "Invalid vector composition assignment"
                );
            }
        }
        let mut removed = vec![false; instructions.len()];
        for c in &self.changes {
            match &c.assignment {
                Some(a) => instructions[c.index].set_display_parameters(
                    DisplayPlane::Interoperability(a.plane.order),
                    a.priority,
                    a.viewing_group,
                ),
                None => removed[c.index] = true,
            }
            match (&c.substitute, &mut instructions[c.index]) {
                (
                    Some(Substitute::Symbol {
                        reference,
                        rotation,
                        rotation_crs,
                        scale,
                    }),
                    DrawingInstruction::Point(p),
                ) => {
                    p.symbol_ref = reference.clone();
                    p.rotation = *rotation;
                    p.rotation_crs = *rotation_crs;
                    p.scale = *scale;
                }
                (Some(Substitute::LineStyle { reference, style }), DrawingInstruction::Line(l)) => {
                    l.style_ref = Some(reference.clone());
                    l.style = (**style).clone();
                }
                (Some(Substitute::AreaFill(template)), DrawingInstruction::Area(a)) => {
                    a.fill = template.fill.clone();
                    a.fill_ref = template.fill_ref.clone();
                    a.fill_color_token = template.fill_color_token.clone();
                    a.fill_opacity = template.fill_opacity;
                    a.hatch_color_token = template.hatch_color_token.clone();
                    a.hatch_line_style_refs = template.hatch_line_style_refs.clone();
                    a.hatch_strokes = template.hatch_strokes.clone();
                }
                (Some(Substitute::Text(t)), DrawingInstruction::Text(x)) => {
                    x.text = t.text.clone();
                    x.font_size = t.size;
                    (x.color, x.color_token, x.color_opacity) =
                        (t.color.0, Some(t.color.1.clone()), t.color.2);
                    match &t.background {
                        Some((c, token, opacity)) => {
                            x.background = Some(*c);
                            x.background_color_token = Some(token.clone());
                            x.background_opacity = *opacity;
                        }
                        None => {
                            x.background = None;
                            x.background_color_token = None;
                        }
                    }
                    x.h_align = t.h_align;
                    x.v_align = t.v_align;
                    x.rotation = t.rotation;
                    x.rotation_crs = ferrite_render::RotationCrs::Portrayal;
                }
                _ => {}
            }
        }
        if removed.contains(&true) {
            let mut index = 0;
            instructions.retain(|_| {
                index += 1;
                !removed[index - 1]
            });
        }
        Ok(())
    }
    /// Instructions removed by Level 2 feature-type suppression.
    pub fn suppressed_len(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| c.assignment.is_none())
            .count()
    }
}
/// Resolve once per (source feature, portrayal primitive), then share among its symbols/text/layers.
/// The caller retains the original PC instructions and can rebuild when IC is disabled/changed.
pub fn plan_interoperability(
    catalogue: &Catalogue,
    cell: &S101Cell,
    fc: &FeatureCatalogue,
    instructions: &[DrawingInstruction],
    cell_index: u32,
) -> Result<InteroperabilityPlan> {
    plan_interoperability_in(
        catalogue,
        Selection::default(),
        cell,
        fc,
        instructions,
        cell_index,
        &|_, _| false,
        None,
    )
}
/// Whether `product` has data coverage over the whole geographic extent
/// [west, south, east, north].
pub type ProductCoverage<'a> = dyn Fn(&str, [f64; 4]) -> bool + 'a;
/// S-98 Part B B-7: feature-type suppression precedes interleaving. Suppression
/// applies only where the replacing products also have coverage (Part B B-5.1);
/// an instance is hidden only when its whole extent is covered, never partially.
#[allow(clippy::too_many_arguments)]
pub fn plan_interoperability_in(
    catalogue: &Catalogue,
    selection: Selection,
    cell: &S101Cell,
    fc: &FeatureCatalogue,
    instructions: &[DrawingInstruction],
    cell_index: u32,
    coverage: &ProductCoverage<'_>,
    portrayal: Option<(&ferrite_portrayal_catalog::PortrayalCatalogue, &str)>,
) -> Result<InteroperabilityPlan> {
    let combination = catalogue.active_combination(selection);
    let mut suppressed: HashMap<i64, bool> = HashMap::new();
    let mut assignments: HashMap<(i64, &str), Option<Assignment>> = HashMap::new();
    let mut changes = Vec::new();
    for (index, i) in instructions
        .iter()
        .enumerate()
        .filter(|(_, i)| i.cell_index() == Some(cell_index))
    {
        let id = i
            .feature_id()
            .context("S-101 portrayal is missing its feature identity")?;
        let feature = cell
            .features
            .get(&id)
            .context("S-101 portrayal references an unknown feature")?;
        let code = feature
            .feature_code
            .as_deref()
            .context("Unmapped feature code")?;
        let layer = catalogue.suppressed_layer(selection, "S-101", code);
        if layer.is_none() && !catalogue.has_rules_for("S-101", code) {
            continue;
        }
        let definition = fc
            .feature_types
            .get(code)
            .with_context(|| format!("Feature code is not in FC: {code}"))?;
        let use_type = definition
            .feature_use_type
            .with_context(|| format!("featureUseType is required before IC processing: {code}"))?;
        // S-98 4.2.2.2: meta features retain their own product's PC portrayal.
        // Do this before evaluating selectors, including selectors with unsupported attributes.
        if use_type == ferrite_feature_catalog::FeatureUseType::Meta {
            continue;
        }
        if let (Some(layer), Some(combination)) = (layer, combination) {
            let hidden = *suppressed.entry(id).or_insert_with(|| {
                // Without replacement references the other included products
                // stand for "another product".
                let others: Vec<&str> = if layer.replacement_products.is_empty() {
                    combination
                        .products
                        .iter()
                        .map(String::as_str)
                        .filter(|p| *p != "S-101")
                        .collect()
                } else {
                    layer
                        .replacement_products
                        .iter()
                        .map(String::as_str)
                        .collect()
                };
                feature_extent(cell, feature)
                    .is_some_and(|extent| others.iter().all(|p| coverage(p, extent)))
            });
            if hidden {
                changes.push(Change {
                    index,
                    cell: cell_index,
                    feature: id,
                    geometry: i.geometry_type(),
                    assignment: None,
                    substitute: None,
                });
                continue;
            }
            if !catalogue.has_rules_for("S-101", code) {
                continue;
            }
        }
        let geometry = drawing_geometry(i);
        if let std::collections::hash_map::Entry::Vacant(entry) = assignments.entry((id, geometry))
        {
            let mut forest = None;
            let assignment = catalogue
                .resolve_portrayal_in(
                    selection,
                    "S-101",
                    code,
                    feature_geometry(feature),
                    geometry,
                    |path| {
                        ensure!(
                            fc.attribute_visibility(code, &path[0]).is_some(),
                            "IC filter root is not bound to feature in FC: {}",
                            path[0]
                        );
                        if forest.is_none() {
                            forest = Some(AttributeForest::new(&feature.attributes)?);
                        }
                        forest.as_ref().unwrap().scalar(path, fc)
                    },
                )
                .with_context(|| format!("IC selection for {code} {id}"))?;
            entry.insert(assignment);
        }
        if let Some(assignment) = assignments[&(id, geometry)].clone() {
            let substitute = match &assignment.substitution {
                Some(substitution) => resolve_substitute(substitution, i, portrayal)
                    .with_context(|| format!("IC substitution for {code} {id}"))?,
                None => None,
            };
            changes.push(Change {
                index,
                cell: cell_index,
                feature: id,
                geometry: i.geometry_type(),
                assignment: Some(assignment),
                substitute,
            });
        }
    }
    Ok(InteroperabilityPlan { changes })
}
/// Build one aggregate immutable plan against the same ORIGINAL whole stream.
/// Each existing product planner uses its own FC and unchanged global cell index.
/// No per-Edition stream split, renumbering, intermediate mutation or re-sort.
pub fn plan_interoperability_for_cells(
    catalogue: &Catalogue,
    cells: &[S101Cell],
    fcs: &[&FeatureCatalogue],
    instructions: &[DrawingInstruction],
) -> Result<InteroperabilityPlan> {
    plan_interoperability_for_cells_in(
        catalogue,
        Selection::default(),
        cells,
        fcs,
        instructions,
        &|_, _| false,
        &[],
    )
}
pub fn plan_interoperability_for_cells_in(
    catalogue: &Catalogue,
    selection: Selection,
    cells: &[S101Cell],
    fcs: &[&FeatureCatalogue],
    instructions: &[DrawingInstruction],
    coverage: &ProductCoverage<'_>,
    portrayal: &[Option<(&ferrite_portrayal_catalog::PortrayalCatalogue, &str)>],
) -> Result<InteroperabilityPlan> {
    ensure!(
        portrayal.is_empty() || portrayal.len() == cells.len(),
        "IC cell portrayal owner count mismatch"
    );
    ensure!(
        cells.len() == fcs.len(),
        "IC cell catalogue owner count mismatch"
    );
    ensure!(
        cells.len() <= 4096,
        "IC cell catalogue owner limit exceeded"
    );
    for instruction in instructions {
        if let Some(index) = instruction.cell_index() {
            ensure!(
                (index as usize) < cells.len(),
                "IC instruction source cell is out of range"
            );
        }
    }
    let mut changes = Vec::new();
    for (index, (cell, fc)) in cells.iter().zip(fcs).enumerate() {
        let plan = plan_interoperability_in(
            catalogue,
            selection,
            cell,
            fc,
            instructions,
            u32::try_from(index)?,
            coverage,
            portrayal.get(index).copied().flatten(),
        )?;
        changes.try_reserve(plan.changes.len())?;
        changes.extend(plan.changes);
    }
    Ok(InteroperabilityPlan { changes })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fc() -> FeatureCatalogue {
        let mut fc = FeatureCatalogue {
            source_path: Default::default(),
            name: "Test".into(),
            scope: String::new(),
            version: "1".into(),
            version_date: String::new(),
            product_id: "S-101".into(),
            simple_attributes: HashMap::new(),
            complex_attributes: HashMap::new(),
            feature_types: HashMap::new(),
            information_types: HashMap::new(),
        };
        fc.simple_attributes.insert(
            "language".into(),
            ferrite_feature_catalog::SimpleAttribute {
                code: "language".into(),
                name: "Language".into(),
                definition: None,
                value_type: AttributeValueType::Text,
                uom: None,
                listed_values: vec![],
                quantitative_range: None,
            },
        );
        fc.complex_attributes.insert(
            "featureName".into(),
            ferrite_feature_catalog::ComplexAttribute {
                code: "featureName".into(),
                name: "Name".into(),
                definition: None,
                sub_attributes: vec![ferrite_feature_catalog::AttributeBinding {
                    attribute_code: "language".into(),
                    visibility: Default::default(),
                    multiplicity: Default::default(),
                    sequential: false,
                    permitted_values: vec![],
                }],
            },
        );
        fc
    }
    fn a(code: &str, parent: u16, value: &str) -> Attribute {
        Attribute {
            natc: 1,
            atix: 99,
            paix: parent,
            atvl: value.into(),
            value: None,
            code: Some(code.into()),
        }
    }
    #[test]
    fn paths_use_parent_positions_and_preserve_text() {
        let attrs = [
            a("language", 0, "root"),
            a("featureName", 0, ""),
            a("language", 2, "eng"),
        ];
        let f = AttributeForest::new(&attrs).unwrap();
        let fc = fc();
        assert_eq!(
            f.scalar(&["featureName".into(), "language".into()], &fc)
                .unwrap(),
            Some(Scalar::Text("eng".into()))
        );
        assert_eq!(
            f.scalar(&["language".into()], &fc).unwrap(),
            Some(Scalar::Text("root".into()))
        );
        let unknown = [a("language", 0, "")];
        assert_eq!(
            AttributeForest::new(&unknown)
                .unwrap()
                .scalar(&["language".into()], &fc)
                .unwrap(),
            None
        );
        let repeated = [a("language", 0, "eng"), a("language", 0, "fra")];
        assert!(AttributeForest::new(&repeated)
            .unwrap()
            .scalar(&["language".into()], &fc)
            .is_err());
        assert!(AttributeForest::new(&[a("x", 2, ""), a("y", 1, "")]).is_err());
        assert!(AttributeForest::new(&[a("x", 9, "")]).is_err());
    }

    fn owner_fixture(
        use_type: ferrite_feature_catalog::FeatureUseType,
    ) -> (FeatureCatalogue, S101Cell) {
        let mut fc = fc();
        fc.feature_types.insert(
            "Wreck".into(),
            ferrite_feature_catalog::FeatureType {
                code: "Wreck".into(),
                name: "Test".into(),
                definition: None,
                is_abstract: false,
                super_type: None,
                feature_use_type: Some(use_type),
                attribute_bindings: vec![],
                information_bindings: vec![],
                feature_bindings: vec![],
                permitted_primitives: vec![],
            },
        );
        let mut cell = S101Cell {
            file_path: Default::default(),
            dsid: Default::default(),
            code_mappings: ferrite_s100_core::DatasetCodeMappings::new(),
            coord_factor: 1.,
            coord_factor_y: 1.,
            coord_factor_z: 1.,
            coord_origin_x: 0.,
            coord_origin_y: 0.,
            coord_origin_z: 0.,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: HashMap::new(),
            multi_points: HashMap::new(),
            curves: HashMap::new(),
            composite_curves: HashMap::new(),
            surfaces: HashMap::new(),
            features: HashMap::new(),
            information: HashMap::new(),
            spatial_information_associations: HashMap::new(),
        };
        cell.features.insert(
            1,
            FeatureRecord {
                frid: ferrite_s100_core::FRID {
                    rcid: 1,
                    nftc: 1,
                    rver: 1,
                    ruin: 1,
                },
                foid: None,
                attributes: vec![],
                spatial_associations: vec![],
                information_associations: vec![],
                feature_associations: vec![],
                masks: vec![],
                feature_code: Some("Wreck".into()),
                primitive_type: SpatialPrimitiveType::Point,
            },
        );
        (fc, cell)
    }
    #[test]
    fn aggregate_owner_plan_preserves_interleaved_indexes_and_per_owner_meta_semantics() {
        use ferrite_feature_catalog::FeatureUseType;
        use ferrite_render::{PointInstruction, WorldPoint};
        let (meta, a) = owner_fixture(FeatureUseType::Meta);
        let (geographic, b) = owner_fixture(FeatureUseType::Geographic);
        let cells = vec![a, b];
        let xml = include_str!("../../ferrite-interoperability/tests/display-plane.xml").replace(
            "<attributeCombination>categoryOfWreck = 1</attributeCombination>",
            "",
        );
        let catalogue = Catalogue::parse(&xml).unwrap();
        let make = |index| {
            DrawingInstruction::Point(
                PointInstruction::new("SAME_RAW_ID".into(), WorldPoint::new(0., 0.))
                    .with_cell_index(index)
                    .with_feature_id(1),
            )
        };
        let original = vec![make(1), make(0), make(1)];
        let mut expected = original.clone();
        plan_interoperability(&catalogue, &cells[0], &meta, &expected, 0)
            .unwrap()
            .apply(&mut expected)
            .unwrap();
        plan_interoperability(&catalogue, &cells[1], &geographic, &expected, 1)
            .unwrap()
            .apply(&mut expected)
            .unwrap();
        let aggregate =
            plan_interoperability_for_cells(&catalogue, &cells, &[&meta, &geographic], &original)
                .unwrap();
        assert_eq!(aggregate.len(), 2);
        let mut actual = original.clone();
        aggregate.apply(&mut actual).unwrap();
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(
            actual
                .iter()
                .map(DrawingInstruction::cell_index)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(0), Some(1)]
        );
        assert_eq!(
            serde_json::to_value(&actual[1]).unwrap(),
            serde_json::to_value(&original[1]).unwrap()
        );
        let mut invalid = geographic;
        invalid
            .feature_types
            .get_mut("Wreck")
            .unwrap()
            .feature_use_type = None;
        assert!(
            plan_interoperability_for_cells(&catalogue, &cells, &[&meta, &invalid], &original)
                .is_err()
        );
        assert!(plan_interoperability_for_cells(&catalogue, &cells, &[&meta], &original).is_err());
        assert!(plan_interoperability_for_cells(
            &catalogue,
            &cells,
            &[&meta, &invalid],
            &[make(2)]
        )
        .is_err());
        assert_eq!(
            original
                .iter()
                .map(DrawingInstruction::cell_index)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(0), Some(1)]
        );
    }
    #[test]
    fn level2_suppression_hides_only_fully_covered_instances_of_the_active_combination() {
        use ferrite_feature_catalog::FeatureUseType;
        use ferrite_render::{PointInstruction, WorldPoint};
        use ferrite_s100_core::{Coordinate, PointRecord, RecordId, SpatialAssociation};
        let (fc, mut cell) = owner_fixture(FeatureUseType::Geographic);
        let point = RecordId::new(110, 7);
        cell.points.insert(
            point.key(),
            PointRecord {
                id: point,
                position: Coordinate::new(129., 35.),
                update_instruction: 1,
            },
        );
        cell.features.get_mut(&1).unwrap().spatial_associations = vec![SpatialAssociation {
            spatial_id: point,
            ornt: 1,
            usag: 1,
            mask: 2,
            scale_minimum: None,
            scale_maximum: None,
            update_instruction: 1,
        }];
        let product = "codeList=\"urn:ferrite:test:products\"";
        let xml = include_str!("../../ferrite-interoperability/tests/display-plane.xml")
            .replace(
                "<attributeCombination>categoryOfWreck = 1</attributeCombination>",
                "",
            )
            .replace("<interoperabilityLevel>1", "<interoperabilityLevel>2")
            .replace(
                "</displayPlanes>",
                &format!(
                    "</displayPlanes><predefinedProductCombinations><S100_IC_PredefinedCombination>\
                     <identifier>pdc</identifier><name>n</name><description>d</description>\
                     <useConditions>u</useConditions><interoperabilityLevel>2</interoperabilityLevel>\
                     <includedProduct {product} codeListValue=\"S-101\">a</includedProduct>\
                     <includedProduct {product} codeListValue=\"S-102\">b</includedProduct>\
                     <suppressedFeatureLayers><S100_IC_SuppressedFeatureLayer><identifier>s</identifier>\
                     <featureCode>Wreck</featureCode><product {product} codeListValue=\"S-101\">a</product>\
                     <featureRef>coverageRule</featureRef></S100_IC_SuppressedFeatureLayer>\
                     </suppressedFeatureLayers></S100_IC_PredefinedCombination></predefinedProductCombinations>"
                ),
            );
        let catalogue = Catalogue::parse(&xml).unwrap();
        let keep = DrawingInstruction::Point(
            PointInstruction::new("OTHER".into(), WorldPoint::new(0., 0.))
                .with_cell_index(1)
                .with_feature_id(9),
        );
        let wreck = DrawingInstruction::Point(
            PointInstruction::new("WRECK".into(), WorldPoint::new(129., 35.))
                .with_cell_index(0)
                .with_feature_id(1),
        );
        let original = vec![wreck.clone(), keep, wreck];
        let covered = |p: &str, e: [f64; 4]| p == "S-102" && e == [129., 35., 129., 35.];
        let run = |selection, coverage: &ProductCoverage<'_>| {
            let plan = plan_interoperability_in(
                &catalogue, selection, &cell, &fc, &original, 0, coverage, None,
            )
            .unwrap();
            let mut out = original.clone();
            plan.apply(&mut out).unwrap();
            (plan.suppressed_len(), out)
        };
        let active = catalogue.select(Some("pdc"), &["S-101", "S-102"]).unwrap();
        let (hidden, out) = run(active, &covered);
        assert_eq!(hidden, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].feature_id(), Some(9));
        // Outside the replacing product's coverage the layer stays visible.
        let (hidden, out) = run(active, &|_, _| false);
        assert_eq!((hidden, out.len()), (0, 3));
        // Without the combination, Level 1 portrayal is unchanged.
        let (hidden, out) = run(Selection::default(), &covered);
        assert_eq!((hidden, out.len()), (0, 3));
        // An unresolved spatial reference is never suppressed.
        cell.points.clear();
        let plan =
            plan_interoperability_in(&catalogue, active, &cell, &fc, &original, 0, &covered, None)
                .unwrap();
        assert_eq!(plan.suppressed_len(), 0);
    }
    #[test]
    fn drawing_instruction_rules_substitute_point_symbols_from_the_owning_pc() {
        use ferrite_feature_catalog::FeatureUseType;
        use ferrite_render::{PointInstruction, WorldPoint};
        let (fc, cell) = owner_fixture(FeatureUseType::Geographic);
        let xml = include_str!("../../ferrite-interoperability/tests/display-plane.xml")
            .replace("<attributeCombination>categoryOfWreck = 1</attributeCombination>", "")
            .replace(
                "<viewingGroup>11010</viewingGroup></S100_IC_DrawingInstruction>",
                "<viewingGroup>11010</viewingGroup><substituteSymbolization><symbol reference=\"WRECKS99\" rotation=\"30\"/></substituteSymbolization></S100_IC_DrawingInstruction>",
            );
        let catalogue = Catalogue::parse(&xml).unwrap();
        let point = DrawingInstruction::Point(
            PointInstruction::new("WRECKS01".into(), WorldPoint::new(0., 0.))
                .with_cell_index(0)
                .with_feature_id(1),
        );
        let mut pc = crate::hatch_resource_tests::catalogue();
        let plan = |pc: Option<(&ferrite_portrayal_catalog::PortrayalCatalogue, &str)>| {
            plan_interoperability_in(
                &catalogue,
                Selection::default(),
                &cell,
                &fc,
                std::slice::from_ref(&point),
                0,
                &|_, _| false,
                pc,
            )
        };
        // The substitute must exist in the product's portrayal catalogue.
        assert!(plan(Some((&pc, "Day"))).is_err());
        assert!(plan(None).is_err());
        pc.symbols.symbols.insert(
            "WRECKS99".into(),
            ferrite_portrayal_catalog::Symbol::new("WRECKS99".into(), "WRECKS99.svg".into()),
        );
        let mut out = vec![point.clone()];
        plan(Some((&pc, "Day"))).unwrap().apply(&mut out).unwrap();
        let DrawingInstruction::Point(p) = &out[0] else {
            panic!()
        };
        assert_eq!(p.symbol_ref, "WRECKS99");
        assert_eq!(p.rotation, 30.);
        assert_eq!(p.rotation_crs, ferrite_render::RotationCrs::Portrayal);
        assert_eq!(p.priority.0, 4);
    }
    #[test]
    fn area_fill_and_text_substitutes_use_the_active_colour_profile() {
        use ferrite_feature_catalog::FeatureUseType;
        use ferrite_portrayal_catalog::{ColorDefinition, ColorProfile, SrgbColor};
        use ferrite_render::{AreaInstruction, TextInstruction, WorldPoint};
        let (fc, cell) = owner_fixture(FeatureUseType::Geographic);
        let xml = include_str!("../../ferrite-interoperability/tests/display-plane.xml")
            .replace("<attributeCombination>categoryOfWreck = 1</attributeCombination>", "")
            .replace("<geometryType>point</geometryType>", "")
            .replace(
                "<viewingGroup>11010</viewingGroup></S100_IC_DrawingInstruction>",
                "<viewingGroup>11010</viewingGroup><substituteSymbolization><colorFill><color transparency=\"0.5\">DEPDW</color></colorFill><textPoint horizontalAlignment=\"End\" verticalAlignment=\"Center\" rotation=\"15\"><element><text>WK</text><bodySize>11</bodySize><foreground>CHBLK</foreground></element></textPoint></substituteSymbolization></S100_IC_DrawingInstruction>",
            );
        let catalogue = Catalogue::parse(&xml).unwrap();
        let area = DrawingInstruction::Area(
            AreaInstruction::new(vec![WorldPoint::new(0., 0.); 3])
                .with_cell_index(0)
                .with_feature_id(1),
        );
        let text = DrawingInstruction::Text(
            TextInstruction::new("old".into(), WorldPoint::new(0., 0.))
                .with_cell_index(0)
                .with_feature_id(1),
        );
        let original = vec![area, text];
        let mut pc = crate::hatch_resource_tests::catalogue();
        let mut day = ColorProfile::new("Day".into(), "Day".into());
        for (token, rgb) in [("DEPDW", (200, 220, 250)), ("CHBLK", (0, 0, 0))] {
            day.colors.insert(
                token.into(),
                ColorDefinition {
                    token: token.into(),
                    srgb: Some(SrgbColor::new(rgb.0, rgb.1, rgb.2)),
                    cie: None,
                },
            );
        }
        let plan = |pc: &ferrite_portrayal_catalog::PortrayalCatalogue| {
            plan_interoperability_in(
                &catalogue,
                Selection::default(),
                &cell,
                &fc,
                &original,
                0,
                &|_, _| false,
                Some((pc, "Day")),
            )
        };
        // Tokens must exist in the active profile.
        assert!(plan(&pc).is_err());
        pc.color_profiles.profiles.insert("Day".into(), day);
        let mut out = original.clone();
        plan(&pc).unwrap().apply(&mut out).unwrap();
        let DrawingInstruction::Area(a) = &out[0] else {
            panic!()
        };
        assert_eq!(a.fill_color_token.as_deref(), Some("DEPDW"));
        assert_eq!(a.fill_opacity, 0.5);
        let DrawingInstruction::Text(t) = &out[1] else {
            panic!()
        };
        assert_eq!((t.text.as_str(), t.font_size), ("WK", 11.));
        assert_eq!(t.color_token.as_deref(), Some("CHBLK"));
        assert_eq!(t.h_align, ferrite_render::HAlign::Right);
        assert_eq!(t.v_align, ferrite_render::VAlign::Middle);
        assert_eq!(t.rotation, 15.);
    }
    #[test]
    fn ambiguous_planes_abort_aggregate_planning_without_changing_portrayal() {
        use ferrite_feature_catalog::FeatureUseType;
        use ferrite_render::{PointInstruction, WorldPoint};
        let (fc, cell) = owner_fixture(FeatureUseType::Geographic);
        let mut xml = include_str!("../../ferrite-interoperability/tests/display-plane.xml")
            .replace(
                "<attributeCombination>categoryOfWreck = 1</attributeCombination>",
                "",
            );
        let start = xml.rfind("<S100_IC_DisplayPlane>").unwrap();
        let end = xml.rfind("</S100_IC_DisplayPlane>").unwrap() + "</S100_IC_DisplayPlane>".len();
        let duplicate = xml[start..end]
            .replace("DangerPlane", "OtherDangerPlane")
            .replace("<name>Danger", "<name>Other danger")
            .replace("wreckRule", "otherWreckRule");
        xml.insert_str(end, &duplicate);
        let catalogue = Catalogue::parse(&xml).unwrap();
        let original = vec![DrawingInstruction::Point(
            PointInstruction::new("ORIGINAL".into(), WorldPoint::new(129., 35.))
                .with_feature_id(1)
                .with_cell_index(0),
        )];
        let before = serde_json::to_value(&original).unwrap();
        let error = match plan_interoperability_for_cells(&catalogue, &[cell], &[&fc], &original) {
            Ok(_) => panic!("ambiguous plane assignment was accepted"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains("Ambiguous IC display-plane match"),
            "{error}"
        );
        assert_eq!(serde_json::to_value(&original).unwrap(), before);
    }

    #[test]
    fn meta_portrayal_is_unchanged_even_when_ic_targets_it() {
        use ferrite_feature_catalog::{FeatureType, FeatureUseType};
        use ferrite_render::{PointInstruction, WorldPoint};
        let mut fc = fc();
        fc.feature_types.insert(
            "Wreck".into(),
            FeatureType {
                code: "Wreck".into(),
                name: "Test".into(),
                definition: None,
                is_abstract: false,
                super_type: None,
                feature_use_type: Some(FeatureUseType::Meta),
                attribute_bindings: vec![],
                information_bindings: vec![],
                feature_bindings: vec![],
                permitted_primitives: vec![],
            },
        );
        let mut cell = S101Cell {
            file_path: Default::default(),
            dsid: Default::default(),
            code_mappings: ferrite_s100_core::DatasetCodeMappings::new(),
            coord_factor: 1.,
            coord_factor_y: 1.,
            coord_factor_z: 1.,
            coord_origin_x: 0.,
            coord_origin_y: 0.,
            coord_origin_z: 0.,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: HashMap::new(),
            multi_points: HashMap::new(),
            curves: HashMap::new(),
            composite_curves: HashMap::new(),
            surfaces: HashMap::new(),
            features: HashMap::new(),
            information: HashMap::new(),
            spatial_information_associations: HashMap::new(),
        };
        cell.features.insert(
            1,
            FeatureRecord {
                frid: ferrite_s100_core::FRID {
                    rcid: 1,
                    nftc: 1,
                    rver: 1,
                    ruin: 1,
                },
                foid: None,
                attributes: vec![],
                spatial_associations: vec![],
                information_associations: vec![],
                feature_associations: vec![],
                masks: vec![],
                feature_code: Some("Wreck".into()),
                primitive_type: SpatialPrimitiveType::Point,
            },
        );
        let mut instructions = vec![DrawingInstruction::Point(
            PointInstruction::new("PC_SYMBOL".into(), WorldPoint::new(0., 0.))
                .with_cell_index(0)
                .with_feature_id(1),
        )];
        let original = serde_json::to_value(&instructions).unwrap();
        let xml = include_str!("../../ferrite-interoperability/tests/display-plane.xml");
        let catalogue = Catalogue::parse(xml).unwrap();
        let plan = plan_interoperability(&catalogue, &cell, &fc, &instructions, 0).unwrap();
        assert!(plan.is_empty());
        plan.apply(&mut instructions).unwrap();
        assert_eq!(serde_json::to_value(&instructions).unwrap(), original);
        fc.feature_types.get_mut("Wreck").unwrap().feature_use_type = None;
        assert!(
            plan_interoperability(&catalogue, &cell, &fc, &instructions, 0)
                .err()
                .unwrap()
                .to_string()
                .contains("featureUseType")
        );
        fc.feature_types.get_mut("Wreck").unwrap().feature_use_type =
            Some(FeatureUseType::Geographic);
        // The same ordinary feature without a filter is eligible for composition.
        let catalogue = Catalogue::parse(&xml.replace(
            "<attributeCombination>categoryOfWreck = 1</attributeCombination>",
            "",
        ))
        .unwrap();
        assert_eq!(
            plan_interoperability(&catalogue, &cell, &fc, &instructions, 0)
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn stale_plan_is_atomic() {
        use ferrite_render::{PointInstruction, WorldPoint};
        let assignment = Assignment {
            plane: ferrite_kernel::CompositionPlane::new(
                ferrite_kernel::CompositionStage::Chart,
                std::num::NonZeroI32::new(-10).unwrap(),
            ),
            priority: 2,
            viewing_group: 11010,
            substitution: None,
        };
        let plan = InteroperabilityPlan {
            changes: vec![
                Change {
                    index: 0,
                    cell: 0,
                    feature: 1,
                    geometry: GeometryType::Point,
                    assignment: Some(assignment.clone()),
                    substitute: None,
                },
                Change {
                    index: 1,
                    cell: 0,
                    feature: 2,
                    geometry: GeometryType::Point,
                    assignment: Some(assignment),
                    substitute: None,
                },
            ],
        };
        let mut instructions = vec![
            DrawingInstruction::Point(
                PointInstruction::new("TEST".into(), WorldPoint::new(0., 0.))
                    .with_cell_index(0)
                    .with_feature_id(1),
            ),
            DrawingInstruction::Point(
                PointInstruction::new("TEST".into(), WorldPoint::new(0., 0.))
                    .with_cell_index(1)
                    .with_feature_id(2),
            ),
        ];
        assert!(plan.apply(&mut instructions).is_err());
        assert_eq!(instructions[0].display_plane(), DisplayPlane::UnderRadar);
    }
    #[test]
    fn numeric_values_use_fc_type_and_original_encoding() {
        let mut fc = fc();
        let mut integer = fc.simple_attributes["language"].clone();
        integer.code = "integer".into();
        integer.value_type = AttributeValueType::Integer;
        fc.simple_attributes.insert("integer".into(), integer);
        let mut real = fc.simple_attributes["language"].clone();
        real.code = "real".into();
        real.value_type = AttributeValueType::Real;
        fc.simple_attributes.insert("real".into(), real);
        let huge = "90071992547409930000000000000000000000000";
        let attributes = [a("integer", 0, huge), a("real", 0, "1E-5")];
        let forest = AttributeForest::new(&attributes).unwrap();
        assert_eq!(
            forest.scalar(&["integer".into()], &fc).unwrap(),
            Some(Scalar::Number(Decimal::parse(huge).unwrap()))
        );
        assert_eq!(
            forest.scalar(&["real".into()], &fc).unwrap(),
            Some(Scalar::Number(Decimal::parse("0.00001").unwrap()))
        );
    }
}
