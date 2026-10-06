//! Product-independent perspective navigation on the WGS84 surface.
use crate::{
    geodesy::{direct, GeographicPosition},
    globe_camera::GlobeCamera,
};
use anyhow::{bail, ensure, Result};
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobePose {
    pub focus: GeographicPosition,
    pub range_m: f64,
    pub heading_deg: f64,
    pub tilt_deg: f64,
}
impl GlobePose {
    pub fn camera(self, viewport: [f64; 2]) -> Result<GlobeCamera> {
        GlobeCamera::orbit(
            self.focus,
            self.range_m,
            self.heading_deg,
            self.tilt_deg,
            viewport,
            45.,
            1.,
            1e8,
        )
    }
    /// Move the orbit focus so an authored surface anchor projects at the target
    /// physical pixel. Failure leaves the caller's camera unchanged.
    pub fn anchored(
        self,
        viewport: [f64; 2],
        anchor: GeographicPosition,
        target: [f64; 2],
        range_m: f64,
    ) -> Result<Self> {
        ensure!(
            target.iter().all(|v| v.is_finite())
                && range_m.is_finite()
                && (10. ..=50_000_000.).contains(&range_m),
            "Invalid globe navigation target"
        );
        let ecef = anchor.to_ecef(0.)?;
        let residual = |pose: Self| -> Result<[f64; 2]> {
            let c = pose.camera(viewport)?;
            let q = c.clip_ecef(ecef)?;
            ensure!(q[3] > 1., "Anchor behind near plane");
            Ok([
                (q[0] / q[3] + 1.) * viewport[0] / 2. - target[0],
                (1. - q[1] / q[3]) * viewport[1] / 2. - target[1],
            ])
        };
        let mut pose = Self { range_m, ..self };
        if residual(pose).is_err() {
            pose.focus = anchor;
        }
        let move_focus = |p: Self, east: f64, north: f64| -> Result<Self> {
            Ok(Self {
                focus: direct(p.focus, east.atan2(north).to_degrees(), east.hypot(north))?,
                ..p
            })
        };
        for _ in 0..40 {
            let r = residual(pose)?;
            let error = r[0].hypot(r[1]);
            if error < 1e-5 {
                ensure!(
                    pose.camera(viewport)?.project_visible(ecef)?.is_some(),
                    "Anchor occluded by ellipsoid"
                );
                return Ok(pose);
            }
            let step = (range_m * 1e-5).clamp(0.01, 100.);
            let a = residual(move_focus(pose, step, 0.)?)?;
            let b = residual(move_focus(pose, 0., step)?)?;
            let j = [
                (a[0] - r[0]) / step,
                (b[0] - r[0]) / step,
                (a[1] - r[1]) / step,
                (b[1] - r[1]) / step,
            ];
            let det = j[0] * j[3] - j[1] * j[2];
            ensure!(
                det.is_finite() && det.abs() > 1e-16,
                "Globe anchor at singular horizon"
            );
            let east = (-r[0] * j[3] + j[1] * r[1]) / det;
            let north = (j[2] * r[0] - j[0] * r[1]) / det;
            let mut accepted = None;
            for k in 0..20 {
                let f = 0.5_f64.powi(k);
                if let Ok(next) = move_focus(pose, east * f, north * f) {
                    if let Ok(q) = residual(next) {
                        if q[0].hypot(q[1]) < error {
                            accepted = Some(next);
                            break;
                        }
                    }
                }
            }
            let Some(next) = accepted else {
                bail!("Globe anchor cannot be retained at requested range")
            };
            pose = next;
        }
        bail!("Globe anchor did not converge")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tilted_dateline_and_high_latitude_anchors_survive_zoom_and_pan() {
        for lat in [0., 70., 89.] {
            for tilt in [0., 45., 70.] {
                let p = GlobePose {
                    focus: GeographicPosition::new(lat, 179.99).unwrap(),
                    range_m: 30000.,
                    heading_deg: 0.,
                    tilt_deg: tilt,
                };
                let v = [1200., 800.];
                let c = p.camera(v).unwrap();
                let s = [730., 510.];
                let a = c.pick(s).unwrap().unwrap().geodetic.surface;
                let mut q = p;
                for scale in [1.15, 2., 10., 200., 20., 1.] {
                    q = q.anchored(v, a, s, 30000. / scale).unwrap();
                    let x = q
                        .camera(v)
                        .unwrap()
                        .project_visible(a.to_ecef(0.).unwrap())
                        .unwrap()
                        .unwrap();
                    assert!((x.screen_px[0] - s[0]).hypot(x.screen_px[1] - s[1]) < 1e-5);
                }
                q = q.anchored(v, a, [800., 550.], q.range_m).unwrap();
                let x = q.camera(v).unwrap().pick([800., 550.]).unwrap().unwrap();
                assert!(
                    crate::geodesy::inverse(a, x.geodetic.surface)
                        .unwrap()
                        .distance_m
                        < 0.001
                );
            }
        }
    }
    #[test]
    fn impossible_space_anchor_and_invalid_input_are_rejected() {
        let p = GlobePose {
            focus: GeographicPosition::new(0., 0.).unwrap(),
            range_m: 2e7,
            heading_deg: 0.,
            tilt_deg: 0.,
        };
        assert!(p.anchored([800., 600.], p.focus, [1e9, 1e9], 2e7).is_err());
        assert!(p
            .anchored([800., 600.], p.focus, [f64::NAN, 0.], 500.)
            .is_err());
    }
}
