//! Machine-readable WGS84 inverse and perspective camera audit.
use ferrite_kernel::{
    geocentric::from_ecef, geodesy::GeographicPosition, globe_camera::GlobeCamera,
};
fn main() {
    let mut args = std::env::args().skip(1);
    let input = args.next().unwrap();
    let output = args.next().unwrap();
    let positions: Vec<[f64; 3]> = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let mut rows = Vec::with_capacity(positions.len());
    for xyz in positions {
        let p = from_ecef(xyz).unwrap();
        rows.push(serde_json::json!({"ecef":xyz,"latitude":p.surface.latitude(),"longitude":p.surface.longitude(),"height":p.ellipsoidal_height_m,"reconstructed":p.to_ecef().unwrap()}));
    }
    let mut cameras = Vec::new();
    for lat in [-90., -80., 0., 48.65, 80., 90.] {
        for lon in [-180., -2.05, 179.9] {
            for range in [100., 10000., 6378137., 25512548.] {
                for tilt in [0., 35., 80.] {
                    for heading in [0., 90., 270.] {
                        let focus = GeographicPosition::new(lat, lon).unwrap();
                        let c = GlobeCamera::orbit(
                            focus,
                            range,
                            heading,
                            tilt,
                            [1600., 900.],
                            45.,
                            0.1,
                            1e9,
                        )
                        .unwrap();
                        let point = focus.to_ecef(0.).unwrap();
                        let s = c.project_visible(point).unwrap().unwrap();
                        let pick = c.pick(s.screen_px).unwrap().unwrap();
                        cameras.push(serde_json::json!({"focus":[lat,lon],"range":range,"tilt":tilt,"heading":heading,"screen":s.screen_px,"clip_depth":s.clip_depth,"picked":[pick.geodetic.surface.latitude(),pick.geodetic.surface.longitude(),pick.geodetic.ellipsoidal_height_m],"ecef_error_m":point.iter().zip(pick.ecef_m).map(|(a,b)|(a-b)*(a-b)).sum::<f64>().sqrt()}));
                    }
                }
            }
        }
    }
    std::fs::write(output,serde_json::to_vec_pretty(&serde_json::json!({"inverse":rows,"camera":cameras,"app_integration_verified":false,"physical_input_verified":false})).unwrap()).unwrap();
}
