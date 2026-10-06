//! CPU evidence for the actual geographic draper -> coverage region -> pixel mask.
use ferrite_kernel::{
    coverage_raster::rasterize, geodesy::GeographicPosition, globe_camera::GlobeCamera,
    globe_coverage_projection::CoverageProjectionLimits,
};
use ferrite_render::{AreaInstruction, WorldPoint};
use ferrite_wgpu::{
    globe_coverage_projection::project_coverage_area, globe_portrayal::DrapingLimits,
};
fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("output"));
    std::fs::create_dir_all(&out).unwrap();
    let mut checks = Vec::new();
    for (index, (lat, lon, heading, tilt)) in [
        (48., 179.99, 0., 0.),
        (48., 179.99, 35., 50.),
        (85., 0., 30., 45.),
    ]
    .into_iter()
    .enumerate()
    {
        let camera = GlobeCamera::orbit(
            GeographicPosition::new(lat, lon).unwrap(),
            30000.,
            heading,
            tilt,
            [640., 480.],
            45.,
            3.,
            1e9,
        )
        .unwrap();
        let ring = |radius: f64| {
            vec![
                WorldPoint::new(lon - radius, lat - radius),
                WorldPoint::new(lon + radius, lat - radius),
                WorldPoint::new(lon + radius, lat + radius),
                WorldPoint::new(lon - radius, lat + radius),
                WorldPoint::new(lon - radius, lat - radius),
            ]
        };
        let mut area = AreaInstruction::new(ring(0.05));
        area.interiors.push(ring(0.01));
        let (region, stats) = project_coverage_area(
            &area,
            &camera,
            DrapingLimits::default(),
            CoverageProjectionLimits::default(),
        )
        .unwrap();
        let mask = rasterize(&region, [640, 480], 640 * 480).unwrap();
        assert!(stats.final_triangles > 0);
        assert!(region.polygons().iter().any(|p| !p.interiors().is_empty()));
        let center = camera
            .project_visible(
                GeographicPosition::new(lat, lon)
                    .unwrap()
                    .to_ecef(0.)
                    .unwrap(),
            )
            .unwrap()
            .unwrap()
            .screen_px;
        assert!(!mask.contains_pixel(center[0].floor() as u32, center[1].floor() as u32));
        let mut pixels = vec![0u8; 640 * 480];
        let mut visible = 0;
        for y in 0..480 {
            for x in 0..640 {
                if mask.contains_pixel(x, y) {
                    pixels[(y * 640 + x) as usize] = 255;
                    visible += 1;
                }
            }
        }
        assert!(visible > 0);
        image::save_buffer(
            out.join(format!("mask-{index}.png")),
            &pixels,
            640,
            480,
            image::ColorType::L8,
        )
        .unwrap();
        checks.push(serde_json::json!({"latitude":lat,"longitude":lon,"heading":heading,"tilt":tilt,"source_vertices":stats.source_vertices,"source_triangles":stats.source_triangles,"final_triangles":stats.final_triangles,"edge_error_px":stats.max_edge_error_px,"chord_error_m":stats.max_edge_chord_error_m,"polygons":region.polygons().len(),"holes":region.polygons().iter().map(|p|p.interiors().len()).sum::<usize>(),"visible_pixels":visible,"cropped_mask_bytes":mask.pixels().len(),"hole_center_hidden":true,"fill_none_source":true,"mask_origin":mask.origin(),"mask_size":mask.size()}));
    }
    std::fs::write(
        out.join("result.json"),
        serde_json::to_string_pretty(&checks).unwrap(),
    )
    .unwrap();
}
