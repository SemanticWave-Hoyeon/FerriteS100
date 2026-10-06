//! Product-neutral screen text deconfliction, independent of fonts and the GPU.
use rstar::{RTree, RTreeObject, AABB};
#[derive(Clone, Copy, Debug)]
pub struct TextFootprint {
    pub corners: [[f32; 2]; 4],
}
impl TextFootprint {
    pub fn rotated(origin: [f32; 2], size: [f32; 2], angle: f32) -> Self {
        let (sin, cos) = angle.sin_cos();
        Self {
            corners: [[0., 0.], [size[0], 0.], size, [0., size[1]]]
                .map(|[x, y]| [origin[0] + x * cos - y * sin, origin[1] + x * sin + y * cos]),
        }
    }
    pub fn bounds(&self) -> [[f32; 2]; 2] {
        let mut min = [f32::INFINITY; 2];
        let mut max = [f32::NEG_INFINITY; 2];
        for p in self.corners {
            for i in 0..2 {
                min[i] = min[i].min(p[i]);
                max[i] = max[i].max(p[i]);
            }
        }
        [min, max]
    }
    fn intersects(&self, other: &Self) -> bool {
        // Separating axis test on both rectangles. Touching edges are allowed.
        for polygon in [self, other] {
            for i in 0..2 {
                let a = polygon.corners[i];
                let b = polygon.corners[i + 1];
                let axis = [-(b[1] - a[1]), b[0] - a[0]];
                let interval = |p: &Self| {
                    let values = p.corners.map(|v| v[0] * axis[0] + v[1] * axis[1]);
                    (
                        values.into_iter().fold(f32::INFINITY, f32::min),
                        values.into_iter().fold(f32::NEG_INFINITY, f32::max),
                    )
                };
                let (a, b) = interval(self);
                let (c, d) = interval(other);
                if b <= c || d <= a {
                    return false;
                }
            }
        }
        true
    }
}
impl RTreeObject for TextFootprint {
    type Envelope = AABB<[f32; 2]>;
    fn envelope(&self) -> Self::Envelope {
        let [min, max] = self.bounds();
        AABB::from_corners(min, max)
    }
}
#[derive(Default)]
pub struct TextPlacement {
    occupied: RTree<TextFootprint>,
}
impl TextPlacement {
    pub fn try_place(&mut self, footprint: TextFootprint) -> bool {
        if !footprint.corners.iter().flatten().all(|v| v.is_finite()) {
            return false;
        }
        if self
            .occupied
            .locate_in_envelope_intersecting(&footprint.envelope())
            .any(|old| footprint.intersects(old))
        {
            return false;
        }
        self.occupied.insert(footprint);
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rstar::Envelope;
    #[test]
    fn rotated_rectangles_use_polygon_not_aabb() {
        let mut placement = TextPlacement::default();
        let a = TextFootprint::rotated([0., 0.], [100., 4.], std::f32::consts::FRAC_PI_4);
        let b = TextFootprint::rotated([-14., 14.], [100., 4.], std::f32::consts::FRAC_PI_4);
        assert!(a.envelope().intersects(&b.envelope()));
        assert!(placement.try_place(a));
        assert!(placement.try_place(b));
        assert!(!placement.try_place(a));
    }
    #[test]
    fn crossing_rotated_text_is_rejected() {
        let mut placement = TextPlacement::default();
        assert!(placement.try_place(TextFootprint::rotated([0., 0.], [100., 4.], 0.)));
        assert!(!placement.try_place(TextFootprint::rotated(
            [50., -50.],
            [100., 4.],
            std::f32::consts::FRAC_PI_2
        )));
    }
    #[test]
    fn touching_text_allowed_and_nonfinite_rejected() {
        let mut placement = TextPlacement::default();
        assert!(placement.try_place(TextFootprint::rotated([0., 0.], [10., 10.], 0.)));
        assert!(placement.try_place(TextFootprint::rotated([10., 0.], [10., 10.], 0.)));
        assert!(!placement.try_place(TextFootprint::rotated([f32::NAN, 0.], [10., 10.], 0.)));
    }
}
