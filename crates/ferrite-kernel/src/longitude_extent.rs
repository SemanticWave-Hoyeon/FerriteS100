//! Directed geographic metadata arcs, independent of product encodings.
//! Endpoints are canonical degrees. Source intervals keep their original sheet;
//! this API never shortens edges or normalizes polygon vertices.
use anyhow::{ensure, Result};
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LongitudeArc { west: f64, east: f64 }
impl LongitudeArc {
    /// Equal endpoints denote a closed singleton, not an entire circle.
    /// Only [-180, +180] denotes the full canonical longitude axis.
    pub fn new(west: f64, east: f64) -> Result<Self> {
        ensure!([west,east].iter().all(|v|v.is_finite() && (-180.0..=180.0).contains(v)), "Noncanonical longitude arc");
        Ok(Self {west,east})
    }
    pub fn crosses_seam(self) -> bool { self.west > self.east }
    /// At most two closed intervals, with no allocation or numeric translation.
    /// A seam singleton includes both coordinate representations of that point.
    pub fn segments(self) -> ([[f64;2];2],usize) {
        if self.crosses_seam() { ([[self.west,180.],[-180.,self.east]],2) }
        else if self.west == -180. && self.east < 180. { ([[-180.,self.east],[180.,180.]],2) }
        else if self.east == 180. && self.west > -180. { ([[self.west,180.],[-180.,-180.]],2) }
        else { ([[self.west,self.east],[0.,0.]],1) }
    }
    /// Whole original interval, not only its endpoints. No periodic shortening.
    /// Unwrapped/native projected inputs must use their own explicit contract.
    pub fn encloses_canonical_interval(self, lower: f64, upper: f64) -> Result<bool> {
        ensure!(lower.is_finite() && upper.is_finite() && lower >= -180. && upper <= 180. && lower <= upper, "Noncanonical continuous interval");
        let (segments,count)=self.segments();
        Ok(segments[..count].iter().any(|s|lower>=s[0] && upper<=s[1]))
    }
}
#[cfg(test)] mod tests {
 use super::*;
 #[test] fn crossing_arc_contains_each_sheet_piece_but_not_the_long_way() {
  let a=LongitudeArc::new(170.,-170.).unwrap();assert!(a.crosses_seam());
  for (l,u) in [(175.,180.),(-180.,-175.),(170.,170.),(-170.,-170.)] {assert!(a.encloses_canonical_interval(l,u).unwrap());}
  for (l,u) in [(-175.,175.),(0.,1.),(169.,175.),(-175.,-169.)] {assert!(!a.encloses_canonical_interval(l,u).unwrap());}
  assert!(a.encloses_canonical_interval(170.,190.).is_err());
 }
 #[test] fn full_axis_and_seam_singletons_are_distinct() {
  let full=LongitudeArc::new(-180.,180.).unwrap();assert!(full.encloses_canonical_interval(-180.,180.).unwrap());
  for (w,e) in [(-180.,-180.),(180.,180.),(180.,-180.)] {
   let a=LongitudeArc::new(w,e).unwrap();assert!(a.encloses_canonical_interval(-180.,-180.).unwrap());assert!(a.encloses_canonical_interval(180.,180.).unwrap());assert!(!a.encloses_canonical_interval(-180.,180.).unwrap());
  }
  assert!(LongitudeArc::new(170.,180.).unwrap().encloses_canonical_interval(-180.,-180.).unwrap());
  assert!(LongitudeArc::new(-180.,-170.).unwrap().encloses_canonical_interval(180.,180.).unwrap());
  let a=LongitudeArc::new(0.,0.).unwrap();assert!(a.encloses_canonical_interval(0.,0.).unwrap());assert!(!a.encloses_canonical_interval(0.,f64::from_bits(1)).unwrap());
 }
 #[test] fn boundaries_have_no_tolerance_and_invalid_inputs_are_rejected() {
  let a=LongitudeArc::new(170.,-170.).unwrap();assert!(!a.encloses_canonical_interval(f64::from_bits(170f64.to_bits()-1),175.).unwrap());
  assert!(!a.encloses_canonical_interval(-175.,f64::from_bits((-170f64).to_bits()-1)).unwrap());
  for (l,u) in [(f64::NAN,0.),(0.,f64::INFINITY),(2.,1.),(-181.,0.),(0.,181.)] {assert!(a.encloses_canonical_interval(l,u).is_err());assert!(LongitudeArc::new(l,u).is_err() || l>u);}
 }
 #[test] fn closed_extent_is_invariant_under_longitude_reflection() {
  for (w,e) in [(170.,-170.),(-180.,180.),(-180.,-170.),(170.,180.),(180.,-180.),(0.,0.),(-25.,80.)] {
   let a=LongitudeArc::new(w,e).unwrap();let reflected=LongitudeArc::new(-e,-w).unwrap();
   for (l,u) in [(-180.,-180.),(180.,180.),(-180.,180.),(-179.,-175.),(175.,179.),(-175.,175.),(-25.,80.),(0.,f64::from_bits(1))] {
    assert_eq!(a.encloses_canonical_interval(l,u).unwrap(),reflected.encloses_canonical_interval(-u,-l).unwrap(),"arc {w}..{e}, interval {l}..{u}");
   }
  }
 }

}
