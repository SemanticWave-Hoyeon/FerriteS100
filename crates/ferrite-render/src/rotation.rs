//! S-100 Part 9 9-12.2.2.7 / Part 9a Rotation coordinate bases.
use crate::{FlatProjection, PointInstruction, Scaler, TextInstruction};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RotationCrs {
    #[default]
    Portrayal,
    Geographic,
    Local,
    Line,
}
impl RotationCrs {
    pub fn from_lua(value: &str) -> Result<Self, String> {
        match value {
            "PortrayalCRS" => Ok(Self::Portrayal),
            "GeographicCRS" => Ok(Self::Geographic),
            "LocalCRS" => Ok(Self::Local),
            "LineCRS" => Ok(Self::Line),
            _ => Err(format!("Unknown rotation CRS: {value}")),
        }
    }
}
/// Resolve clockwise screen degrees. Geographic angles are projected as a full
/// bearing (tilt need not preserve angles); Local/Line angles rotate the rigid
/// millimetre coordinate frame about its projected curve tangent.
pub fn screen_rotation(
    point: &PointInstruction,
    direction: impl Fn(f64) -> Result<[f64; 2], String>,
) -> Result<f32, String> {
    rotation_basis(
        point.rotation,
        point.rotation_crs,
        point.curve_tangent_bearing,
        direction,
    )
}
/// Resolve a text basis through any map or perspective direction adapter.
pub fn screen_text_rotation(
    text: &TextInstruction,
    direction: impl Fn(f64) -> Result<[f64; 2], String>,
) -> Result<f32, String> {
    rotation_basis(
        text.rotation,
        text.rotation_crs,
        text.curve_tangent_bearing,
        direction,
    )
}
fn rotation_basis(
    rotation: f32,
    crs: RotationCrs,
    tangent: Option<f64>,
    direction: impl Fn(f64) -> Result<[f64; 2], String>,
) -> Result<f32, String> {
    let rotation = rotation as f64;

    if !rotation.is_finite() {
        return Err("Non-finite portrayal rotation".into());
    }
    let angle = |v: [f64; 2]| -> Result<f64, String> {
        if v.iter().any(|x| !x.is_finite()) || v[0].hypot(v[1]) <= 0. {
            return Err("Singular projected rotation basis".into());
        }
        Ok(v[0].atan2(-v[1]).to_degrees())
    };
    let value = match crs {
        RotationCrs::Portrayal => rotation,
        RotationCrs::Geographic => angle(direction(rotation)?)?,
        RotationCrs::Local | RotationCrs::Line => {
            if let Some(bearing) = tangent {
                let t = direction(bearing)?;
                angle([t[1], -t[0]])? + rotation
            } else if crs == RotationCrs::Local {
                rotation
            } else {
                return Err("LineCRS portrayal lacks source curve tangent".into());
            }
        }
    };
    Ok(value.rem_euclid(360.) as f32)
}
/// Analytic WGS84 east/north projection derivative; avoid differencing f32
/// screen coordinates or crossing the longitude wrap at the dateline.
pub fn flat_rotation(point: &PointInstruction, scaler: &Scaler) -> Result<f32, String> {
    flat_rotation_basis(
        if point.portrayal_origin.is_device_fixed() {
            scaler
                .screen_to_world(
                    point
                        .portrayal_origin
                        .flat_glyph_anchor(point.position, scaler)
                        .map_err(|e| e.to_string())?,
                )
                .y
        } else {
            point.position.y
        },
        point.rotation,
        point.rotation_crs,
        point.curve_tangent_bearing,
        scaler,
    )
}
/// Shared point/text projection basis.
pub fn flat_text_rotation(text: &TextInstruction, scaler: &Scaler) -> Result<f32, String> {
    flat_rotation_basis(
        if text.portrayal_origin.is_device_fixed() {
            scaler
                .screen_to_world(
                    text.portrayal_origin
                        .flat_glyph_anchor(text.position, scaler)
                        .map_err(|e| e.to_string())?,
                )
                .y
        } else {
            text.position.y
        },
        text.rotation,
        text.rotation_crs,
        text.curve_tangent_bearing,
        scaler,
    )
}
fn flat_rotation_basis(
    latitude: f64,
    rotation: f32,
    crs: RotationCrs,
    tangent: Option<f64>,
    scaler: &Scaler,
) -> Result<f32, String> {
    rotation_basis(rotation, crs, tangent, |bearing| {
        use ferrite_kernel::geodesy::{WGS84_A, WGS84_F};
        if !latitude.is_finite() || latitude.abs() >= 90. || !bearing.is_finite() {
            return Err("Invalid flat rotation basis".into());
        }
        let (s, c) = latitude.to_radians().sin_cos();
        let e2 = WGS84_F * (2. - WGS84_F);
        let q = 1. - e2 * s * s;
        let n = WGS84_A / q.sqrt();
        let m = WGS84_A * (1. - e2) / q.powf(1.5);
        let derivative = match scaler.projection() {
            FlatProjection::LocalGeographic => 1.,
            FlatProjection::EllipsoidalMercator => (1. - e2) / (c * q),
        };
        let (sb, cb) = bearing.to_radians().sin_cos();
        Ok([
            scaler.scale_x() * (sb / (n * c)).to_degrees(),
            -scaler.scale_y() * (cb / m).to_degrees() * derivative,
        ])
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GeoBounds, Viewport, WorldPoint};
    #[test]
    fn geographic_bearing_is_projected_instead_of_adding_north_angle() {
        let p = PointInstruction::new("A".into(), WorldPoint::new(0., 70.))
            .with_rotation(45.)
            .with_rotation_crs(RotationCrs::Geographic);
        let a = screen_rotation(&p, |b| {
            let (s, c) = b.to_radians().sin_cos();
            Ok([s, -c * 0.5])
        })
        .unwrap();
        assert!((a - 63.43495).abs() < 1e-4);
        let mut s = Scaler::new(GeoBounds::new(-1., 69., 1., 71.), Viewport::new(800., 600.));
        s.set_viewport(Viewport::new(800., 600.));
        s.set_projection(FlatProjection::EllipsoidalMercator);
        s.set_bounds(GeoBounds::new(-1., 69., 1., 71.));
        assert!((flat_rotation(&p, &s).unwrap() - 45.).abs() < 1e-4);
    }
    #[test]
    fn curve_normal_and_reversal_are_preserved_and_missing_line_is_rejected() {
        let p = PointInstruction::new("A".into(), WorldPoint::new(0., 0.))
            .with_rotation(30.)
            .with_rotation_crs(RotationCrs::Line);
        assert!(screen_rotation(&p, |_| Ok([1., 0.])).is_err());
        let p = p.with_curve_tangent(Some(90.));
        assert!((screen_rotation(&p, |_| Ok([1., 0.])).unwrap() - 30.).abs() < 1e-5);
        assert!((screen_rotation(&p, |_| Ok([-1., 0.])).unwrap() - 210.).abs() < 1e-5);
        assert!(RotationCrs::from_lua("misspelled").is_err());
        let bytes = bincode::serialize(&p).unwrap();
        let restored: PointInstruction = bincode::deserialize(&bytes).unwrap();
        assert_eq!(restored.rotation_crs, RotationCrs::Line);
        assert_eq!(restored.curve_tangent_bearing, Some(90.));
    }
}
