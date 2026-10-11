//! Opt-in hidden regression controls for the actual PC -> renderer point path.
//! Expected digit/quality groups are authored from the official PC, not from a
//! renderer winner/dedup helper. No interactive execution or visibility policy.
use super::*;
use ferrite_render::{DisplayPlane, PointInstruction, ScaleRange, ViewingGroup};

fn official_groups(
    pc: &BoundPortrayalCatalogue,
    fc: &BoundFeatureCatalogue,
) -> Result<Vec<Vec<String>>> {
    let mut engine = PortrayalEngine::new_with_sources(pc.sources())?;
    engine.set_type_catalogue(TypeCatalogue::from_feature_catalogue(fc));
    engine.initialize()?;
    let groups: Vec<Vec<String>> = engine.session().eval(
        r#"
        require 'SOUNDG03'
        local cases = {
            {'4', false, false}, {'4.1', false, false}, {'14.1', false, false},
            {'-1.7', false, false}, {'4.2', true, true}, {'12', false, true}
        }
        local groups = {}
        for index, row in ipairs(cases) do
            local feature = {
                Code = 'Sounding',
                techniqueOfVerticalMeasurement = row[2] and {4} or {},
                qualityOfVerticalMeasurement = row[3] and {3} or {},
                status = {},
                MultiPoint = {Points = {{X = 1, Y = 1, ScaledZ = StringToScaledDecimal(row[1])}}}
            }
            function feature:GetSpatialAssociation()
                return {GetInformationAssociation = function() return nil end}
            end
            local emitted = {}
            local portrayal = {AddInstructions = function(_, text) emitted[#emitted+1] = text end}
            SOUNDG03(feature, portrayal, {
                SafetyDepth = StringToScaledDecimal('10'),
                SafetyContour = StringToScaledDecimal('30')
            }, 33010)
            local group = {}
            for _, text in ipairs(emitted) do
                local symbol = string.match(text, '^PointInstruction:(.*)$')
                if symbol then group[#group+1] = symbol end
            end
            groups[index] = group
        end
        return groups
    "#,
    )?;
    let expected = [
        vec!["SOUNDS14", "SOUNDS50"],
        vec!["SOUNDS14", "SOUNDS51"],
        vec!["SOUNDG21", "SOUNDG14", "SOUNDG51"],
        vec!["SOUNDSA1", "SOUNDS11", "SOUNDS57"],
        vec!["SOUNDSB1", "SOUNDSC3", "SOUNDS14", "SOUNDS52"],
        vec!["SOUNDGC2", "SOUNDG11", "SOUNDG02"],
    ];
    anyhow::ensure!(
        groups.len() == expected.len(),
        "PC fixture group count changed"
    );
    for (i, (actual, wanted)) in groups.iter().zip(expected).enumerate() {
        anyhow::ensure!(
            actual.iter().map(String::as_str).eq(wanted),
            "Official PC sounding group {i} differs from authored expectation: {actual:?}"
        );
    }
    Ok(groups)
}

impl ChartApp {
    pub(super) fn audit_point_retention(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden point audit required"
        );
        self.ensure_navigation_scene()?;
        let groups = official_groups(&self.pc, &self.fc)?;
        let scaler = self.render_context.scaler.clone();
        let centre = scaler.screen_to_world(scaler.viewport.center());
        let centre = WorldPoint::new(
            (centre.x * 1e6).floor() / 1e6 + 1e-7,
            (centre.y * 1e6).floor() / 1e6 + 1e-7,
        );
        let mut fixtures = Vec::new();
        // Different members deliberately share a feature and exact coordinates.
        // Near positions also collide under the removed 1e6 coordinate quantizer.
        for (group, refs) in groups.iter().enumerate() {
            for name in refs {
                let mut p = PointInstruction::new(
                    name.clone(),
                    WorldPoint::new(centre.x + if group == 1 { 1e-7 } else { 0. }, centre.y),
                );
                p.feature_id = Some(42);
                p.cell_index = Some(0);
                p.priority = ferrite_render::DisplayPriority(18);
                p.viewing_group = ViewingGroup(33010);
                fixtures.push(p);
            }
        }
        let base = fixtures[0].clone();
        // Exact duplicate commands are still commands. Distinct scale, plane,
        // priority, offset, rotation and source identity are independently kept.
        fixtures.push(base.clone());
        for variant in 0..7 {
            let mut p = base.clone();
            match variant {
                0 => p.scale = 2.,
                1 => p.display_plane = DisplayPlane::OverRadar,
                2 => p.priority = ferrite_render::DisplayPriority(19),
                3 => p.local_offset = (0.5, 0.25),
                4 => p.rotation = 15.,
                5 => p.feature_id = Some(43),
                _ => p.cell_index = Some(1),
            }
            fixtures.push(p);
        }
        let profile = self
            .get_current_profile()
            .context("Missing audit colour profile")?
            .clone();
        let previous_show = self
            .renderer
            .as_ref()
            .context("No renderer")?
            .show_soundings;
        let previous_zoom = self.renderer.as_ref().unwrap().zoom_level;
        let result = (|| -> Result<()> {
            fs::create_dir_all(output)?;
            let mut summary = Vec::new();
            for (name, zoom, animation, enabled) in [
                ("stationary", 20., false, true),
                ("animated", 20., true, true),
                ("below-old-threshold", 44.999, false, true),
                ("above-old-threshold", 45., true, true),
                ("soundings-off", 20., false, false),
            ] {
                let mut context = RenderContext::new(scaler.viewport);
                context.scaler = scaler.clone();
                for point in &fixtures {
                    context.add_instruction(DrawingInstruction::Point(point.clone()));
                }
                // Canonical PointInstruction stream has ascending priority and
                // stable command ordinals for ties. Plane remains separate on
                // every instance and is composed by the renderer draw schedule.
                let mut ordered = fixtures.clone();
                ordered.sort_by_key(|p| p.priority.0);
                let owner = self.symbol_cache.resource_revision();
                let expected = if enabled {
                    ordered.iter().map(|p| serde_json::json!({
                    "symbol_ref":p.symbol_ref, "feature_id":p.feature_id, "cell_index":p.cell_index,
                    "resource_owner":owner, "source_ordinal":null,
                    "world_bits":[p.position.x.to_bits(),p.position.y.to_bits()],
                    "scale_bits":p.scale.to_bits(),"rotation_bits":p.rotation.to_bits(),
                    "priority":p.priority.0,"plane":p.display_plane.composition_plane(ferrite_kernel::CompositionStage::Chart),
                })).collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let case = output.join(name);
                fs::create_dir_all(&case)?;
                fs::write(
                    case.join("expected.json"),
                    serde_json::to_vec_pretty(&serde_json::json!({"rows":expected}))?,
                )?;
                let renderer = self.renderer.as_mut().unwrap();
                renderer.show_soundings = enabled;
                renderer.set_zoom_level(zoom);
                renderer.set_animation_mode(animation);
                renderer.begin_frame_ex(animation);
                renderer.try_add_instructions_with_symbols(
                    &mut context,
                    Some(&mut self.symbol_cache),
                    Some(&profile),
                    None,
                )?;
                renderer.render()?;
                renderer.audit_point_preservation(&case)?;
                renderer.audit_geometry_buffers(&case.join("gpu"))?;
                renderer.save_screenshot(case.join("frame.png"))?;
                let actual: serde_json::Value =
                    serde_json::from_slice(&fs::read(case.join("point-preservation.json"))?)?;
                let rows = actual["rows"].as_array().context("Missing point rows")?;
                anyhow::ensure!(
                    rows.len() == expected.len(),
                    "{name}: point command multiplicity changed"
                );
                for (ordinal, (want, got)) in expected.iter().zip(rows).enumerate() {
                    for key in [
                        "symbol_ref",
                        "feature_id",
                        "cell_index",
                        "resource_owner",
                        "source_ordinal",
                        "world_bits",
                        "scale_bits",
                        "rotation_bits",
                        "priority",
                        "plane",
                    ] {
                        anyhow::ensure!(
                            want[key] == got[key],
                            "{name} command {ordinal}: {key} changed"
                        );
                    }
                    anyhow::ensure!(
                        got["quad_eligible"] == true,
                        "Eligible central point has no quad"
                    );
                }
                summary.push(serde_json::json!({"case":name,"commands":expected.len(),"actual_instances":rows.len(),"zoom_label":zoom,"actual_camera_unchanged":true,"animation":animation}));
            }
            // Separate ScaleMinimum and group controls exercise real admission.
            for name in ["scale-excluded", "group-excluded"] {
                let mut p = base.clone();
                p.symbol_ref = "WRECKS01".into();
                p.viewing_group = ViewingGroup(91000);
                if name == "scale-excluded" {
                    p.scale_range = ScaleRange {
                        scale_minimum: Some(0),
                        scale_maximum: None,
                    };
                }
                let mut context = RenderContext::new(scaler.viewport);
                context.scaler = scaler.clone();
                context.add_instruction(DrawingInstruction::Point(p));
                let renderer = self.renderer.as_mut().unwrap();
                renderer.show_soundings = true;
                renderer.begin_frame();
                let visible = std::collections::HashSet::new();
                renderer.try_add_instructions_with_symbols(
                    &mut context,
                    Some(&mut self.symbol_cache),
                    Some(&profile),
                    if name == "group-excluded" {
                        Some(&visible)
                    } else {
                        None
                    },
                )?;
                anyhow::ensure!(
                    renderer.displayed_symbols().is_empty(),
                    "{name} bypassed current-view admission"
                );
                summary.push(serde_json::json!({"case":name,"actual_instances":0}));
            }
            fs::write(
                output.join("receipt.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "passed":true,"bound_pc_digest":self.pc.source_digest(),"official_pc_groups":groups,
                    "cases":summary,"hidden":true,"focused":false,
                    "scope":"Original PC sounding symbols and actual renderer command/quad retention. Corrected pixels intentionally differ from old thinned output. No physical FPS or full compliance claim; fixture uses no coverage binding. Real mixed-chart coverage/animation is a separate native gate."
                }))?,
            )?;
            Ok(())
        })();
        if let Some(renderer) = &mut self.renderer {
            renderer.show_soundings = previous_show;
            renderer.set_zoom_level(previous_zoom);
            renderer.set_animation_mode(false);
        }
        self.update_view();
        result
    }
}
