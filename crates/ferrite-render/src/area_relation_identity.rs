//! Exact area topology admission, independent of source allocation lifetimes.
use crate::{DrawingInstruction, WorldPoint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticAreaGeometryEpoch(u64);
impl StaticAreaGeometryEpoch {
    pub(crate) fn fresh() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_update(std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed, |v| v.checked_add(1))
            .expect("Area geometry epoch exhausted"))
    }
}

/// One process-start policy shared by renderer allocation and publication admission.
/// Missing enables the qualified path; explicit0 and all invalid values disable it.
pub fn area_triangulation_reuse_enabled() -> bool {
    let value=std::env::var("FERRITE_AREA_TRIANGULATION_REUSE");
    area_triangulation_reuse_policy(value.as_deref())
}
fn area_triangulation_reuse_policy(value: Result<&str, &std::env::VarError>) -> bool {
    match value {
        Ok(value) => value == "1",
        Err(std::env::VarError::NotPresent) => true,
        Err(std::env::VarError::NotUnicode(_)) => false,
    }
}

const MAX_SLOTS: usize = 1_048_576;
const MAX_POINTS: usize = 16_777_216;
const MAX_RINGS: usize = 1_048_576;
fn ring(a: &[WorldPoint], b: &[WorldPoint], points: &mut usize) -> bool {
    if a.len() != b.len() {return false}
    let Some(n) = points.checked_add(a.len()) else {return false};
    if n > MAX_POINTS {return false} *points = n;
    a.iter().zip(b).all(|(a,b)| a.x.to_bits()==b.x.to_bits() && a.y.to_bits()==b.y.to_bits())
}
pub(crate) fn same_area_inputs(a: &[DrawingInstruction], b: &[DrawingInstruction]) -> bool {
    if a.len()!=b.len() || a.len()>MAX_SLOTS {return false}
    let mut points=0usize; let mut rings=0usize;
    a.iter().zip(b).all(|(a,b)| {
        // Keep complete source slot binding even where no triangulation is needed.
        if std::mem::discriminant(a)!=std::mem::discriminant(b)
            || a.feature_id()!=b.feature_id() || a.cell_index()!=b.cell_index()
            || !crate::line_relation_identity::origin(a.portrayal_origin(),b.portrayal_origin()) {return false}
        let (DrawingInstruction::Area(a),DrawingInstruction::Area(b))=(a,b) else {return true};
        if a.priority!=b.priority || a.display_plane!=b.display_plane
            || a.interiors.len()!=b.interiors.len() {return false}
        let Some(n)=rings.checked_add(1).and_then(|n|n.checked_add(a.interiors.len())) else {return false};
        if n>MAX_RINGS {return false} rings=n;
        ring(&a.exterior,&b.exterior,&mut points)
            && a.interiors.iter().zip(&b.interiors).all(|(a,b)|ring(a,b,&mut points))
    })
}

#[cfg(test)] mod tests {
    use super::*;
    use crate::{AreaInstruction, RenderContext, Viewport, Color, DisplayPlane};
    #[test] fn shared_default_policy_is_exact_and_fail_closed() {
        assert!(area_triangulation_reuse_policy(Err(&std::env::VarError::NotPresent)));
        assert!(area_triangulation_reuse_policy(Ok("1")));
        for value in ["0","","true","false","2"," 1","1 "] {
            assert!(!area_triangulation_reuse_policy(Ok(value)));
        }
        let error=std::env::VarError::NotUnicode(std::ffi::OsString::from("invalid marker"));
        assert!(!area_triangulation_reuse_policy(Err(&error)));
    }
    #[cfg(unix)]
    #[test] fn non_unicode_error_is_not_missing_default() {
        use std::os::unix::ffi::OsStringExt;
        let error=std::env::VarError::NotUnicode(std::ffi::OsString::from_vec(vec![0xff]));
        assert!(!area_triangulation_reuse_policy(Err(&error)));
    }
    fn fixture() -> Vec<DrawingInstruction> {
        let mut a=AreaInstruction::new(vec![WorldPoint::new(0.,0.),WorldPoint::new(8.,0.),WorldPoint::new(0.,8.)]);
        a.interiors=vec![vec![WorldPoint::new(1.,1.),WorldPoint::new(2.,1.),WorldPoint::new(1.,2.)]];
        vec![DrawingInstruction::Area(a.with_feature_id(10).with_cell_index(1))]
    }
    fn ctx(v:Vec<DrawingInstruction>)->RenderContext {let mut c=RenderContext::new(Viewport::new(800.,600.));c.set_instructions_from_cache(v);c.get_sorted_instructions();c}
    #[test] fn exact_owned_geometry_can_share_epoch_but_not_lifetime() {
        let old=ctx(fixture());let mut next=ctx(fixture());let lifetime=next.geometry_revision();
        assert!(next.inherit_static_area_geometry_from(&old));
        assert_eq!(next.static_area_geometry_epoch(),old.static_area_geometry_epoch());
        assert_eq!(lifetime,next.geometry_revision());assert_ne!(lifetime,old.geometry_revision());
        next.remap_colors(&|_|Color::RED);next.set_viewport(1600.,900.);
        assert_eq!(next.static_area_geometry_epoch(),old.static_area_geometry_epoch());
    }
    #[test] fn hole_bits_order_plane_and_source_binding_reject() {
        let old=fixture();
        for mode in 0..9 {let mut new=fixture();if let DrawingInstruction::Area(a)=&mut new[0] {match mode {
            0=>a.exterior[0].x=-0.,1=>a.interiors[0][0].x+=0.1,2=>a.interiors[0].reverse(),
            3=>a.exterior.reverse(),4=>a.display_plane=DisplayPlane::OverRadar,
            5=>a.feature_id=Some(11),6=>a.cell_index=Some(2),7=>a.interiors.clear(),8=>a.priority=crate::DisplayPriority(9),_=>unreachable!()}}
            assert!(!same_area_inputs(&old,&new),"mutation{mode}");}
    }
    #[test] fn replacement_removal_unsorted_and_rejection_bits_preserved() {
        let old=ctx(fixture());let mut new=old.empty_for_rebuild();new.set_instructions_from_cache(fixture());
        assert!(!new.inherit_static_area_geometry_from(&old));new.get_sorted_instructions();assert!(new.inherit_static_area_geometry_from(&old));
        let e=new.static_area_geometry_epoch();new.truncate_instructions(0);assert_ne!(e,new.static_area_geometry_epoch());
        let mut bad=fixture();if let DrawingInstruction::Area(a)=&mut bad[0] {a.exterior[0].x=f64::from_bits(0x7ff8000000000001);}
        assert!(same_area_inputs(&bad,&bad));let mut changed=bad.clone();if let DrawingInstruction::Area(a)=&mut changed[0] {a.exterior[0].x=f64::from_bits(0x7ff8000000000002);}
        assert!(!same_area_inputs(&bad,&changed));
    }
    #[test] fn holes_and_slots_cannot_be_reordered_or_added() {
        let mut a=fixture();if let DrawingInstruction::Area(v)=&mut a[0] {v.interiors.push(vec![WorldPoint::new(3.,3.),WorldPoint::new(4.,3.),WorldPoint::new(3.,4.)]);}
        let mut b=a.clone();if let DrawingInstruction::Area(v)=&mut b[0] {v.interiors.reverse();}assert!(!same_area_inputs(&a,&b));
        let mut b=a.clone();b.push(a[0].clone());assert!(!same_area_inputs(&a,&b));
        let old=ctx(a.clone());let mut b=a;b.push(DrawingInstruction::Area(AreaInstruction::new(vec![WorldPoint::new(0.,0.),WorldPoint::new(1.,0.),WorldPoint::new(0.,1.)])));let new=ctx(b);assert_ne!(old.static_area_geometry_epoch(),new.static_area_geometry_epoch());
    }
    #[test] fn work_limit_is_conservative_and_checked() {
        let mut p=MAX_POINTS;assert!(!ring(&[WorldPoint::new(0.,0.)],&[WorldPoint::new(0.,0.)],&mut p));
        let mut p=usize::MAX;assert!(!ring(&[WorldPoint::new(0.,0.)],&[WorldPoint::new(0.,0.)],&mut p));
    }
}
