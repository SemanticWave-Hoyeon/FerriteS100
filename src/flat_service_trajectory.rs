// Diagnostic path sampling; not a rendering, culling or coverage predicate.
pub fn valid_bounds(b:[f64;4])->bool {b.iter().all(|n|n.is_finite()) && b[2]>b[0] && b[3]>b[1] && b[2]-b[0]<=360.0}
pub fn pan(b:[f64;4],t:f64,outside:bool)->(f64,f64) {
    let w=b[2]-b[0]; let h=b[3]-b[1];let angle=t*std::f64::consts::TAU;
    if outside {(2.0*w+40.0*angle.sin(),10.0*angle.cos())}
    else {(0.2*w*angle.sin(),0.2*h*angle.cos())}
}
pub fn overlap_fraction(b:[f64;4],v:[f64;4])->f64 {
    if !valid_bounds(b)||!valid_bounds(v){return 0.0;}
    let mut area=0.0f64;
    for shift in [-360.0,0.0,360.0] {
        let w=(v[2].min(b[2]+shift)-v[0].max(b[0]+shift)).max(0.0);
        let h=(v[3].min(b[3])-v[1].max(b[1])).max(0.0);
        area+=w*h;
    }
    (area/((v[2]-v[0])*(v[3]-v[1]))).clamp(0.0,1.0)
}
#[cfg(test)]mod tests {
 use super::*;
 #[test]fn primary_pan_is_loaded_extent_bounded(){let b=[-3.,48.,4.,52.];for i in 0..100 {let (x,y)=pan(b,i as f64/99.,false);assert!(x.abs()<=1.4+f64::EPSILON*8.);assert!(y.abs()<=0.8+f64::EPSILON*8.);}}
 #[test]fn overlap_accounts_wrap_edges_and_holes_are_not_claimed(){let b=[0.,0.,10.,10.];assert_eq!(overlap_fraction(b,b),1.);assert_eq!(overlap_fraction(b,[5.,0.,15.,10.]),0.5);assert_eq!(overlap_fraction(b,[10.,0.,20.,10.]),0.);assert_eq!(overlap_fraction(b,[360.,0.,370.,10.]),1.);assert_eq!(overlap_fraction(b,[0.,0.,0.,1.]),0.);assert_eq!(overlap_fraction(b,[f64::NAN,0.,1.,1.]),0.);}
 #[test]fn outside_path_is_distinct_not_a_primary_pan(){let b=[0.,0.,1.,1.];let a=pan(b,0.,false);let o=pan(b,0.,true);assert_eq!(a,(0.,0.2));assert_eq!(o,(2.,10.));}
}
