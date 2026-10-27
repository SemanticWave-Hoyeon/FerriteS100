//! Native route overlay tests that load the declared S-421 editing resources
//! through `ferrite-s421`.
#![cfg(test)]
mod tests {
    use crate::native_route::load_native_route_overlay_resources;
    use ferrite_gpu::PreparedNativeRouteOverlay;
    use ferrite_product_api::route_overlay::leg::{sample_leg, MAX_LEG_VERTICES};
    use ferrite_product_api::route_overlay::{
        NativeRouteLegDeclaration, NativeRoutePath, NativeRouteWaypoint, NativeRouteWorldSamples,
        MAX_WORLD_SAMPLE_BYTES,
    };
    use ferrite_render::{Scaler, WorldPoint};
    fn point(id: u32, x: f64, y: f64) -> NativeRouteWaypoint {
        NativeRouteWaypoint {
            waypoint_id: id,
            longitude: x,
            latitude: y,
        }
    }
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "ferrite-native-own-pc-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            for directory in ["Symbols", "LineStyles", "ColorProfiles"] {
                std::fs::create_dir(root.join(directory)).unwrap();
            }
            // Original public catalogue and resource bytes, no invented plane
            // metadata. Only the three declared resources consumed here exist.
            std::fs::write(
                root.join("portrayal_catalogue.xml"),
                include_bytes!("../../ferrite-s421/tests/fixtures/editing/portrayal_catalogue.xml"),
            )
            .unwrap();
            for (file, bytes) in [
                (
                    "Symbols/RTEWPT01.svg",
                    include_bytes!("../../ferrite-s421/tests/fixtures/editing/RTEWPT01.svg")
                        .as_slice(),
                ),
                (
                    "LineStyles/RTEACTLEGLINE.xml",
                    include_bytes!("../../ferrite-s421/tests/fixtures/editing/RTEACTLEGLINE.xml")
                        .as_slice(),
                ),
                (
                    "ColorProfiles/colorProfile.xml",
                    include_bytes!("../../ferrite-s421/tests/fixtures/editing/colorProfile.xml")
                        .as_slice(),
                ),
            ] {
                std::fs::write(root.join(file), bytes).unwrap();
            }
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn immutable_world_samples_match_original_solver_and_do_not_recompute_for_cameras() {
        use ferrite_product_api::route_overlay::NativeRouteLegGeometry;
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = load_native_route_overlay_resources(&fixture.0, "Day").unwrap();
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, 1., 70.), point(3, 3., 70.)],
        }];
        let legs = [NativeRouteLegDeclaration {
            route_id: 7,
            from: 9,
            to: 3,
            geometry: NativeRouteLegGeometry::Orthodrome,
            source_profile: 1,
            source_gml_id: "LEG.1".into(),
            original_geometry_text: "2".into(),
        }];
        let world = NativeRouteWorldSamples::prepare(&paths, &legs, 11).unwrap();
        let legacy = sample_leg([1., 70.], [3., 70.], legs[0].geometry, MAX_LEG_VERTICES).unwrap();
        let actual = world.samples.get(&(7, 3)).unwrap();
        assert_eq!(actual.len(), legacy.len());
        assert!(actual
            .iter()
            .zip(&legacy)
            .all(|(a, b)| a[0].to_bits() == b[0].to_bits() && a[1].to_bits() == b[1].to_bits()));
        assert_eq!(world.solver_invocations(), 1);
        let calls = ferrite_product_api::route_overlay::leg::solver_call_count();
        for (width, x) in [(800., 0.), (1000., 0.1)] {
            let scaler = Scaler::new(
                GeoBounds::new(x, 68., 4. + x, 72.),
                Viewport::new(width, 600.),
            );
            let packet = PreparedNativeRouteOverlay::prepare_with_world_samples(
                resources.clone(),
                11,
                &paths,
                &legs,
                &world,
                &scaler,
                scaler.pixels_per_mm(),
            )
            .unwrap();
            packet
                .validate(&resources, 11, &scaler, scaler.pixels_per_mm())
                .unwrap();
        }
        assert_eq!(
            ferrite_product_api::route_overlay::leg::solver_call_count(),
            calls
        );
        assert!(world.payload_bytes() <= MAX_WORLD_SAMPLE_BYTES);
        let mut altered = paths.clone();
        altered[0].waypoints[1].latitude = 70.00000000000001;
        assert!(!world.matches(&altered, &legs, 11));
        assert!(!world.matches(&paths, &legs, 12));
        let mut profile = legs.clone();
        profile[0].source_profile = 2;
        assert!(!world.matches(&paths, &profile, 11));
        let mut lexical = legs.clone();
        lexical[0].original_geometry_text = "02".into();
        assert!(!world.matches(&paths, &lexical, 11));
    }
    #[test]
    fn route_overlay_extension_projects_like_its_world_samples() {
        use ferrite_product_api::route_overlay::{self, NativeRouteLegGeometry};
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = load_native_route_overlay_resources(&fixture.0, "Day").unwrap();
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, 1., 70.), point(3, 3., 70.), point(4, 3.5, 70.5)],
        }];
        let legs = [NativeRouteLegDeclaration {
            route_id: 7,
            from: 9,
            to: 3,
            geometry: NativeRouteLegGeometry::Orthodrome,
            source_profile: 0,
            source_gml_id: String::new(),
            original_geometry_text: String::new(),
        }];
        let world = NativeRouteWorldSamples::prepare(&paths, &legs, 5).unwrap();
        let scaler = Scaler::new(GeoBounds::new(0., 68., 4., 72.), Viewport::new(800., 600.));
        let expected = PreparedNativeRouteOverlay::prepare_with_world_samples(
            resources.clone(),
            5,
            &paths,
            &legs,
            &world,
            &scaler,
            scaler.pixels_per_mm(),
        )
        .unwrap();
        let extension = route_overlay::extension(world.clone());
        assert_eq!(extension.kind, route_overlay::ROUTE_OVERLAY_EXTENSION);
        let actual = PreparedNativeRouteOverlay::from_extension(
            resources.clone(),
            &extension,
            &scaler,
            scaler.pixels_per_mm(),
        )
        .unwrap();
        actual
            .validate(&resources, 5, &scaler, scaler.pixels_per_mm())
            .unwrap();
        let bits = |p: &ferrite_render::ScreenPoint| (p.x.to_bits(), p.y.to_bits());
        assert_eq!(actual.paths().len(), expected.paths().len());
        for (a, b) in actual.paths().iter().zip(expected.paths()) {
            assert_eq!(a.route_id, b.route_id);
            assert_eq!(a.segment_owners, b.segment_owners);
            assert!(a
                .waypoints
                .iter()
                .zip(&b.waypoints)
                .all(|(a, b)| a.0 == b.0 && bits(&a.1) == bits(&b.1)));
            assert!(a
                .line_points
                .iter()
                .zip(&b.line_points)
                .all(|(a, b)| bits(a) == bits(b)));
            assert_eq!(a.line_points.len(), b.line_points.len());
        }
        let other = ferrite_product_api::RendererExtension {
            kind: "S-102/bathymetry-tiles",
            payload: world,
        };
        assert!(PreparedNativeRouteOverlay::from_extension(
            resources,
            &other,
            &scaler,
            scaler.pixels_per_mm()
        )
        .is_err());
    }
    #[test]
    fn semantic_packet_has_curve_vertices_original_pick_owners_and_stale_camera_guard() {
        use ferrite_product_api::route_overlay::NativeRouteLegGeometry;
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = load_native_route_overlay_resources(&fixture.0, "Day").unwrap();
        let scaler = Scaler::new(
            GeoBounds::new(-35., 65., 35., 77.),
            Viewport::new(800., 600.),
        );
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, -30., 70.), point(3, 30., 70.)],
        }];
        let leg = NativeRouteLegDeclaration {
            route_id: 7,
            from: 9,
            to: 3,
            geometry: NativeRouteLegGeometry::Orthodrome,
            source_profile: 1,
            source_gml_id: "LEG.1".into(),
            original_geometry_text: "2".into(),
        };
        let packet = PreparedNativeRouteOverlay::prepare_with_leg_semantics(
            resources.clone(),
            11,
            &paths,
            &[leg.clone()],
            &scaler,
            scaler.pixels_per_mm(),
        )
        .unwrap();
        let path = &packet.paths()[0];
        assert!(path.line_points.len() > 2);
        assert_eq!(path.segment_owners.len() + 1, path.line_points.len());
        assert!(path.segment_owners.iter().all(|owner| *owner == (9, 3)));
        assert_eq!(path.waypoints.len(), 2);
        assert!(path.line_points[path.line_points.len() / 2].y < path.waypoints[0].1.y);
        let other = Scaler::new(
            GeoBounds::new(-34., 65., 36., 77.),
            Viewport::new(800., 600.),
        );
        assert!(packet
            .validate(&resources, 11, &other, other.pixels_per_mm())
            .is_err());
        let reversed = NativeRouteLegDeclaration {
            from: 3,
            to: 9,
            ..leg
        };
        assert!(PreparedNativeRouteOverlay::prepare_with_leg_semantics(
            resources,
            11,
            &paths,
            &[reversed],
            &scaler,
            scaler.pixels_per_mm()
        )
        .is_err());
    }
    #[test]
    fn declared_seam_packet_uses_continuous_copy_and_legacy_still_declines() {
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = load_native_route_overlay_resources(&fixture.0, "Day").unwrap();
        let scaler = Scaler::new(
            GeoBounds::new(178., 58., 182., 62.),
            Viewport::new(800., 600.),
        );
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, 179., 60.), point(3, -179., 60.)],
        }];
        assert!(PreparedNativeRouteOverlay::prepare(
            resources.clone(),
            11,
            &paths,
            &scaler,
            scaler.pixels_per_mm()
        )
        .is_err());
        let legs = [NativeRouteLegDeclaration {
            route_id: 7,
            from: 9,
            to: 3,
            geometry: ferrite_product_api::route_overlay::NativeRouteLegGeometry::Loxodrome,
            source_profile: 1,
            source_gml_id: "LEG.1".into(),
            original_geometry_text: "1".into(),
        }];
        let packet = PreparedNativeRouteOverlay::prepare_with_leg_semantics(
            resources.clone(),
            11,
            &paths,
            &legs,
            &scaler,
            scaler.pixels_per_mm(),
        )
        .unwrap();
        let expected = scaler.world_to_screen(WorldPoint::new(181., 60.));
        assert_eq!(
            packet.paths()[0].waypoints[1].1.x.to_bits(),
            expected.x.to_bits()
        );
        assert_eq!(
            paths[0].waypoints[1].longitude.to_bits(),
            (-179_f64).to_bits()
        );
        let opposite = Scaler::new(
            GeoBounds::new(-182., 58., -178., 62.),
            Viewport::new(800., 600.),
        );
        let world = NativeRouteWorldSamples::prepare(&paths, &legs, 11).unwrap();
        let opposite_packet = PreparedNativeRouteOverlay::prepare_with_world_samples(
            resources,
            11,
            &paths,
            &legs,
            &world,
            &opposite,
            opposite.pixels_per_mm(),
        )
        .unwrap();
        let opposite_path = &opposite_packet.paths()[0];
        for (i, longitude) in [-181., -179.].into_iter().enumerate() {
            assert_eq!(
                opposite_path.waypoints[i].1,
                opposite.world_to_screen(WorldPoint::new(longitude, 60.))
            );
        }
        assert!(opposite_path
            .line_points
            .iter()
            .all(|p| p.x >= 0. && p.x <= 800.));
        assert!(world.matches(&paths, &legs, 11));
    }
    #[test]
    fn own_public_assets_prepare_exact_camera_and_refuse_stale_candidates() {
        use ferrite_render::{GeoBounds, Viewport};
        let fixture = Fixture::new();
        let resources = load_native_route_overlay_resources(&fixture.0, "Day").unwrap();
        assert_eq!(resources.actual_version(), "");
        assert_eq!(resources.palette(), "Day");
        assert_eq!(resources.leg_style().pen.width, 0.64);
        assert_eq!(resources.leg_style().dashes[0].start, 2.2);
        assert!(resources.waypoint_symbol().pixels.iter().any(|v| *v != 0));
        assert!(load_native_route_overlay_resources(&fixture.0, "Night").is_err());
        let scaler = Scaler::new(GeoBounds::new(-1., 49., 3., 52.), Viewport::new(800., 600.));
        let paths = [NativeRoutePath {
            route_id: 7,
            waypoints: vec![point(9, 2., 50.), point(3, 1., 51.)],
        }];
        let packet = PreparedNativeRouteOverlay::prepare(
            resources.clone(),
            11,
            &paths,
            &scaler,
            scaler.pixels_per_mm(),
        )
        .unwrap();
        for ((id, screen), original) in packet.paths()[0].waypoints.iter().zip(&paths[0].waypoints)
        {
            let expected =
                scaler.world_to_screen(WorldPoint::new(original.longitude, original.latitude));
            assert_eq!(*id, original.waypoint_id);
            assert_eq!(
                [screen.x.to_bits(), screen.y.to_bits()],
                [expected.x.to_bits(), expected.y.to_bits()]
            );
        }
        packet
            .validate(&resources, 11, &scaler, scaler.pixels_per_mm())
            .unwrap();
        assert!(packet
            .validate(&resources, 12, &scaler, scaler.pixels_per_mm())
            .is_err());
        assert!(packet
            .validate(&resources, 11, &scaler, scaler.pixels_per_mm() * 2.)
            .is_err());
        let other_owner = load_native_route_overlay_resources(&fixture.0, "Day").unwrap();
        assert!(packet
            .validate(&other_owner, 11, &scaler, scaler.pixels_per_mm())
            .is_err());
        let invalid_view = Scaler::new(GeoBounds::new(-1., 49., 3., 52.), Viewport::new(0., 600.));
        assert!(PreparedNativeRouteOverlay::prepare(
            resources.clone(),
            11,
            &paths,
            &invalid_view,
            scaler.pixels_per_mm()
        )
        .is_err());
        let mut changed = scaler.clone();
        changed.pan(1., 0.);
        assert!(packet
            .validate(&resources, 11, &changed, changed.pixels_per_mm())
            .is_err());
        // Filesystem mutations cannot change resources already captured by owner.
        let digest = *resources.source_digest();
        std::fs::write(fixture.0.join("Symbols/RTEWPT01.svg"), b"broken").unwrap();
        assert_eq!(*resources.source_digest(), digest);
        assert!(load_native_route_overlay_resources(&fixture.0, "Day").is_err());
        packet
            .validate(&resources, 11, &scaler, scaler.pixels_per_mm())
            .unwrap();
    }
}
