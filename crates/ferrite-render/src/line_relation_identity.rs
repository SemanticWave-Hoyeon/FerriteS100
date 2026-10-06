//! Exact source-only line-relation identity; never a lifetime/pointer cache key.
use crate::{DrawingInstruction,PortrayalPath,PortrayalOrigin,PointOriginGeometry,WorldPoint};
/// Read-only opaque identity. Only a context can issue or inherit it after comparison.
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct StaticLineRelationEpoch(u64);
impl StaticLineRelationEpoch {pub(crate) fn fresh()->Self {static NEXT:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(1);Self(NEXT.fetch_update(std::sync::atomic::Ordering::Relaxed,std::sync::atomic::Ordering::Relaxed,|v|v.checked_add(1)).expect("Static line relation epoch exhausted"))}}
const MAX_COMMANDS:usize=1_048_576;
const MAX_POINTS:usize=16_777_216;
const MAX_PATH_NODES:usize=4096;
fn xy(a:(f64,f64),b:(f64,f64))->bool{a.0.to_bits()==b.0.to_bits()&&a.1.to_bits()==b.1.to_bits()}
fn world(a:WorldPoint,b:WorldPoint)->bool{xy((a.x,a.y),(b.x,b.y))}
fn origin(a:&PortrayalOrigin,b:&PortrayalOrigin)->bool{
 match(a,b){
  (PortrayalOrigin::Unspecified,PortrayalOrigin::Unspecified)|(PortrayalOrigin::NonPoint,PortrayalOrigin::NonPoint)|(PortrayalOrigin::CoverageExempt,PortrayalOrigin::CoverageExempt)=>true,
  (PortrayalOrigin::Point(a),PortrayalOrigin::Point(b))=>match(&**a,&**b){
   (PointOriginGeometry::FeaturePoint(a),PointOriginGeometry::FeaturePoint(b))=>world(*a,*b),
   (PointOriginGeometry::AugmentedPoint{crs:a,coordinates:x},PointOriginGeometry::AugmentedPoint{crs:b,coordinates:y})=>a==b&&xy((x[0],x[1]),(y[0],y[1])),
   (PointOriginGeometry::AugmentedLocalPoint{reference_point:a,millimetres:x},PointOriginGeometry::AugmentedLocalPoint{reference_point:b,millimetres:y})=>world(*a,*b)&&xy((x[0],x[1]),(y[0],y[1])),_=>false},_=>false}
}
fn points(a:&[WorldPoint],b:&[WorldPoint],budget:&mut usize)->bool{
 if a.len()!=b.len(){return false}let Some(next)=budget.checked_add(a.len())else{return false};if next>MAX_POINTS{return false}*budget=next;a.iter().zip(b).all(|(a,b)|world(*a,*b))
}
fn path(a:&PortrayalPath,b:&PortrayalPath,point_budget:&mut usize,node_budget:&mut usize)->bool{
 let mut stack=vec![(a,b,0usize)];
 while let Some((a,b,depth))=stack.pop(){
  if depth>32{return false}let Some(n)=node_budget.checked_add(1)else{return false};if n>MAX_PATH_NODES{return false}*node_budget=n;
  let equal=match(a,b){
   (PortrayalPath::Group(a),PortrayalPath::Group(b))=>{if a.len()!=b.len()||a.len()>MAX_PATH_NODES.saturating_sub(*node_budget).saturating_sub(stack.len()){return false}stack.extend(a.iter().zip(b).rev().map(|(a,b)|(a,b,depth+1)));true},
   (PortrayalPath::Polyline(a),PortrayalPath::Polyline(b))=>{if a.len()!=b.len(){return false}let Some(n)=point_budget.checked_add(a.len())else{return false};if n>MAX_POINTS{return false}*point_budget=n;a.iter().zip(b).all(|(a,b)|xy(*a,*b))},
   (PortrayalPath::GeographicArc{center:a,radius_m:r,start:s,sweep:w},PortrayalPath::GeographicArc{center:b,radius_m:t,start:u,sweep:v})=>xy(*a,*b)&&[r.to_bits(),s.to_bits(),w.to_bits()]==[t.to_bits(),u.to_bits(),v.to_bits()],
   (PortrayalPath::Arc{center:a,radius:r,start:s,sweep:w,geographic_angle:g},PortrayalPath::Arc{center:b,radius:t,start:u,sweep:v,geographic_angle:h})=>xy(*a,*b)&&g==h&&[r.to_bits(),s.to_bits(),w.to_bits()]==[t.to_bits(),u.to_bits(),v.to_bits()],
   (PortrayalPath::Arc3{start:a,median:b,end:c},PortrayalPath::Arc3{start:d,median:e,end:f})=>xy(*a,*d)&&xy(*b,*e)&&xy(*c,*f),
   (PortrayalPath::Annulus{center:a,outer:r,inner:s,start:w,sweep:q,geographic_angle:g},PortrayalPath::Annulus{center:b,outer:t,inner:u,start:v,sweep:z,geographic_angle:h})=>xy(*a,*b)&&g==h&&[r.to_bits(),s.to_bits(),w.to_bits(),q.to_bits()]==[t.to_bits(),u.to_bits(),v.to_bits(),z.to_bits()],_=>false};
  if !equal{return false}
 }true
}
pub(crate) fn same_static_line_relation_inputs(a:&[DrawingInstruction],b:&[DrawingInstruction])->bool{
 if a.len()!=b.len()||a.len()>MAX_COMMANDS{return false}
 let mut point_budget=0;let mut node_budget=0;
 a.iter().zip(b).all(|(a,b)|{
  // Ordinal binding and provenance are kept even for non-line placeholders.
  if std::mem::discriminant(a)!=std::mem::discriminant(b)||a.feature_id()!=b.feature_id()||a.cell_index()!=b.cell_index()||!origin(a.portrayal_origin(),b.portrayal_origin()){return false}
  let (DrawingInstruction::Line(a),DrawingInstruction::Line(b))=(a,b)else{return true};
  if a.priority!=b.priority||a.display_plane!=b.display_plane||a.suppressible!=b.suppressible||!points(&a.points,&b.points,&mut point_budget){return false}
  let rays=match(a.screen_ray,b.screen_ray){(None,None)=>true,(Some(a),Some(b))=>xy((a.direction,a.length_mm),(b.direction,b.length_mm))&&a.geographic_direction==b.geographic_direction,_=>false};
  rays&&match(&a.portrayal_path,&b.portrayal_path){(None,None)=>true,(Some(a),Some(b))=>path(a,b,&mut point_budget,&mut node_budget),_=>false}
 })
}
#[cfg(test)]mod tests{
 use super::*;use crate::{RenderContext,Viewport,LineInstruction,LineSuppressionCache,FlatProjection,Color,DisplayPlane,ScreenRay};
 fn fixture()->Vec<DrawingInstruction>{vec![DrawingInstruction::Line(LineInstruction::new(vec![WorldPoint::new(0.,0.),WorldPoint::new(10.,0.)]).with_priority(1).with_feature_id(10).with_cell_index(0)),DrawingInstruction::Line(LineInstruction::new(vec![WorldPoint::new(2.,0.),WorldPoint::new(8.,0.)]).with_priority(9).with_feature_id(20).with_cell_index(0))]}
 fn context(items:Vec<DrawingInstruction>)->RenderContext{let mut c=RenderContext::new(Viewport::new(800.,600.));c.set_instructions_from_cache(items);c.get_sorted_instructions();c}
 #[test]fn exact_sorted_clone_inherits_relation_not_lifetime(){let old=context(fixture());let mut next=context(fixture());let lifetime=next.geometry_revision();assert_ne!(next.static_line_relation_epoch(),old.static_line_relation_epoch());assert!(next.inherit_static_line_relations_from(&old));assert_eq!(next.static_line_relation_epoch(),old.static_line_relation_epoch());assert_eq!(next.geometry_revision(),lifetime);assert_ne!(lifetime,old.geometry_revision());}
 #[test]fn palette_alpha_and_live_visibility_are_not_cached(){let old=context(fixture());let mut changed=fixture();if let DrawingInstruction::Line(l)=&mut changed[1]{l.style.color=Color::TRANSPARENT;l.style.width=0.;l.viewing_group=crate::ViewingGroup(999);}
  let mut next=context(changed);assert!(next.inherit_static_line_relations_from(&old));let mut cache=LineSuppressionCache::default();let first=cache.plan_context_projected_with_visibility(&old,0,None,None,Some(&[true,true]));assert!(first.partial.contains_key(&0));
  for visible in [[true,true],[true,false],[false,true]]{let actual=cache.plan_context_projected_with_visibility(&next,0,None,None,Some(&visible));let expected=LineSuppressionCache::default().plan_projected_with_visibility(next.raw_instructions(),0,None,None,Some(&visible),next.scaler.projection());assert_eq!(*actual,*expected);assert!(!actual.partial.contains_key(&0));}
 }
 #[test]fn source_points_order_priority_plane_suppression_deferred_and_provenance_reject(){let old=context(fixture());
  for mode in 0..9{let mut items=fixture();if let DrawingInstruction::Line(l)=&mut items[0]{match mode{0=>l.points[0].x=-0.,1=>l.points[1].x+=1.,2=>l.priority=crate::DisplayPriority(2),3=>l.display_plane=DisplayPlane::OverRadar,4=>l.suppressible=false,5=>l.screen_ray=Some(ScreenRay{direction:0.,length_mm:3.,geographic_direction:false}),6=>l.portrayal_path=Some(crate::PortrayalPath::Polyline(vec![(0.,0.),(1.,1.)])),7=>l.feature_id=Some(11),8=>l.cell_index=Some(1),_=>unreachable!()}}
   let mut next=context(items);let before=next.static_line_relation_epoch();assert!(!next.inherit_static_line_relations_from(&old),"mutation{mode}");assert_eq!(next.static_line_relation_epoch(),before);
  }
  let mut items=fixture();if let DrawingInstruction::Line(l)=&mut items[1]{l.priority=crate::DisplayPriority(1);}let old=context(items.clone());items.reverse();let mut reordered=context(items);assert!(!reordered.inherit_static_line_relations_from(&old));
 }
 #[test]fn identical_deferred_payloads_compare_bits_and_decline_changed_geometry(){let mut a=fixture();if let DrawingInstruction::Line(l)=&mut a[0]{l.portrayal_path=Some(PortrayalPath::Group(vec![PortrayalPath::Arc{center:(0.,0.),radius:1.,start:2.,sweep:3.,geographic_angle:false}]));}
  let old=context(a.clone());let mut next=context(a.clone());assert!(next.inherit_static_line_relations_from(&old));if let DrawingInstruction::Line(l)=&mut a[0]{l.portrayal_path=Some(PortrayalPath::Group(vec![PortrayalPath::Arc{center:(-0.,0.),radius:1.,start:2.,sweep:3.,geographic_angle:false}]));}let mut changed=context(a);assert!(!changed.inherit_static_line_relations_from(&old));
 }
 #[test]fn unsorted_and_ownership_mutations_invalidate_but_material_view_changes_do_not(){let old=context(fixture());let mut next=old.empty_for_rebuild();next.set_instructions_from_cache(fixture());assert!(!next.inherit_static_line_relations_from(&old));next.get_sorted_instructions();assert!(next.inherit_static_line_relations_from(&old));let epoch=next.static_line_relation_epoch();next.set_viewport(900.,700.);next.remap_colors(&|_|Color::RED);assert_eq!(epoch,next.static_line_relation_epoch());
  next.set_portrayal_origin_from(0,PortrayalOrigin::CoverageExempt);assert_ne!(epoch,next.static_line_relation_epoch());let e=next.static_line_relation_epoch();next.remove_coverage_exempt_instructions();assert_ne!(e,next.static_line_relation_epoch());
  next.set_instructions_from_cache(fixture());next.get_sorted_instructions();assert!(next.inherit_static_line_relations_from(&old));let e=next.static_line_relation_epoch();next.truncate_instructions(1);assert_ne!(e,next.static_line_relation_epoch());next.clear_instructions();assert_ne!(e,next.static_line_relation_epoch());
 }
 #[test]fn context_and_legacy_namespaces_cannot_alias_and_projection_remains_keyed(){let mut current=context(fixture());let mut wrong=fixture();if let DrawingInstruction::Line(l)=&mut wrong[1]{l.points=vec![WorldPoint::new(20.,0.),WorldPoint::new(30.,0.)];}
  let mut cache=LineSuppressionCache::default();cache.plan_immutable_projected_with_visibility(&wrong,current.static_line_relation_epoch().0,0,None,None,None,FlatProjection::LocalGeographic);
  for projection in [FlatProjection::LocalGeographic,FlatProjection::EllipsoidalMercator]{current.scaler.set_projection(projection);let actual=cache.plan_context_projected_with_visibility(&current,0,None,None,None);let expected=LineSuppressionCache::default().plan_projected_with_visibility(current.raw_instructions(),0,None,None,None,projection);assert_eq!(*actual,*expected);assert!(actual.partial.contains_key(&0));}
  cache.clear();assert!(cache.current().is_none());
 }
 #[test]fn nonline_slots_preserve_full_ordinal_and_source_binding(){
  let mut a=fixture();a.insert(1,DrawingInstruction::Area(crate::AreaInstruction::new(vec![WorldPoint::new(0.,0.),WorldPoint::new(1.,0.),WorldPoint::new(0.,1.)]).with_feature_id(30).with_cell_index(1)));
  let mut b=a.clone();assert!(same_static_line_relation_inputs(&a,&b));
  if let DrawingInstruction::Area(area)=&mut b[1]{area.feature_id=Some(31);}assert!(!same_static_line_relation_inputs(&a,&b));
  b=a.clone();b.remove(1);assert!(!same_static_line_relation_inputs(&a,&b));
  b=a.clone();b.swap(0,1);assert!(!same_static_line_relation_inputs(&a,&b));
 }
 #[test]fn comparison_work_bounds_decline_without_epoch_admission(){let group=PortrayalPath::Group((0..MAX_PATH_NODES+1).map(|_|PortrayalPath::Polyline(vec![])).collect());let mut nodes=0;let mut points=0;assert!(!path(&group,&group,&mut points,&mut nodes));let mut points=MAX_POINTS;assert!(!super::points(&[WorldPoint::new(0.,0.)],&[WorldPoint::new(0.,0.)],&mut points));}
}
