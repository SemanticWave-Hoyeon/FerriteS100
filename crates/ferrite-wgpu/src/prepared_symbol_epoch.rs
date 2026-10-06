//! One successful resource-preparation identity. No camera, visibility or geometry
//! output is cached here: they are recomputed by the original pane path.
use ferrite_portrayal_catalog::ColorProfile;
const KEY_BUDGET:usize=1024*1024;
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) struct Inputs {
    pub source_epoch:u64,pub instruction_count:usize,pub resource_revision:u64,
    pub ppm_bits:u64,pub whole_motifs:bool,pub request_keys:bool,
}
#[derive(Debug,PartialEq,Eq)]
struct ColorIdentity {key:String,token:String,srgb:Option<[u8;3]>,cie:Option<[u64;3]>}
impl ColorIdentity {
 fn matches(&self,p:&ColorProfile)->bool {p.colors.get(&self.key).is_some_and(|c|self.token==c.token&&self.srgb==c.srgb.map(|v|[v.r,v.g,v.b])&&self.cie==c.cie.map(|v|[v.x.to_bits(),v.y.to_bits(),v.l.to_bits()]))}
}
pub(crate) struct PreparedSymbolEpoch {
 inputs:Inputs,profile:Option<(String,String,Vec<ColorIdentity>)>,bytes:usize,
 pub(crate) pattern_payload_bytes:usize,
}
fn copy_bounded(s:&str,bytes:&mut usize,budget:usize)->Option<String> {
 if bytes.checked_add(s.len())?>budget {return None;}
 let mut out=String::new();out.try_reserve_exact(s.len()).ok()?;out.push_str(s);
 *bytes=bytes.checked_add(out.capacity())?;if *bytes>budget {return None;}Some(out)
}
impl PreparedSymbolEpoch {
 pub(crate) fn capture(inputs:Inputs,profile:Option<&ColorProfile>,pattern_payload_bytes:usize)->Option<Self> {Self::capture_budget(inputs,profile,pattern_payload_bytes,KEY_BUDGET)}
 fn capture_budget(inputs:Inputs,profile:Option<&ColorProfile>,pattern_payload_bytes:usize,budget:usize)->Option<Self> {
  let mut bytes=std::mem::size_of::<Self>();if bytes>budget {return None;}
  let profile=if let Some(p)=profile {
   let id=copy_bounded(&p.id,&mut bytes,budget)?;let name=copy_bounded(&p.name,&mut bytes,budget)?;
   let conservative=p.colors.len().checked_mul(std::mem::size_of::<ColorIdentity>())?;if bytes.checked_add(conservative)?>budget{return None;}
   let mut colors=Vec::new();colors.try_reserve_exact(p.colors.len()).ok()?;
   bytes=bytes.checked_add(colors.capacity().checked_mul(std::mem::size_of::<ColorIdentity>())?)?;if bytes>budget{return None;}
   for (key,c) in &p.colors {
    colors.push(ColorIdentity{key:copy_bounded(key,&mut bytes,budget)?,token:copy_bounded(&c.token,&mut bytes,budget)?,srgb:c.srgb.map(|v|[v.r,v.g,v.b]),cie:c.cie.map(|v|[v.x.to_bits(),v.y.to_bits(),v.l.to_bits()])});
   }
   Some((id,name,colors))
  }else{None};Some(Self{inputs,profile,bytes,pattern_payload_bytes})
 }
 pub(crate) fn matches(&self,inputs:Inputs,profile:Option<&ColorProfile>)->bool {
  if self.inputs!=inputs{return false;}
  match (&self.profile,profile) {
   (None,None)=>true,(Some((id,name,colors)),Some(p))=>id==&p.id&&name==&p.name&&colors.len()==p.colors.len()&&colors.iter().all(|c|c.matches(p)),_=>false,
  }
 }
 pub(crate) fn bytes(&self)->usize{self.bytes}
}
#[cfg(test)]mod tests {
 use super::*;use ferrite_portrayal_catalog::{ColorDefinition,SrgbColor,CieColor};
 fn inputs()->Inputs{Inputs{source_epoch:11,instruction_count:20,resource_revision:30,ppm_bits:3.5f64.to_bits(),whole_motifs:false,request_keys:true}}
 fn profile()->ColorProfile{let mut p=ColorProfile::new("id".into(),"name".into());p.colors.insert("C".into(),ColorDefinition{token:"authored".into(),srgb:Some(SrgbColor::new(1,2,3)),cie:Some(CieColor{x:0.,y:0.3,l:40.})});p}
 #[test]fn exact_palette_including_float_bits_and_none_is_required(){let p=profile();let e=PreparedSymbolEpoch::capture(inputs(),Some(&p),100).unwrap();assert!(e.matches(inputs(),Some(&p)));let mut q=p.clone();q.colors.get_mut("C").unwrap().cie.as_mut().unwrap().x=-0.;assert!(!e.matches(inputs(),Some(&q)));q=p.clone();q.colors.get_mut("C").unwrap().srgb.as_mut().unwrap().r=2;assert!(!e.matches(inputs(),Some(&q)));q=p.clone();q.colors.get_mut("C").unwrap().token="different".into();assert!(!e.matches(inputs(),Some(&q)));assert!(!e.matches(inputs(),None));assert!(PreparedSymbolEpoch::capture(inputs(),None,0).unwrap().matches(inputs(),None));}
 #[test]fn every_dependency_invalidates_without_camera_dependency(){let p=profile();let i=inputs();let e=PreparedSymbolEpoch::capture(i,Some(&p),100).unwrap();for changed in [Inputs{source_epoch:i.source_epoch+1,..i},Inputs{instruction_count:i.instruction_count+1,..i},Inputs{resource_revision:i.resource_revision+1,..i},Inputs{ppm_bits:(3.500001f64).to_bits(),..i},Inputs{whole_motifs:true,..i},Inputs{request_keys:false,..i}]{assert!(!e.matches(changed,Some(&p)));}assert!(e.matches(i,Some(&p)));}
 #[test]fn budget_failure_is_cold_fallback_and_does_not_mutate_palette(){let p=profile();assert!(PreparedSymbolEpoch::capture_budget(inputs(),Some(&p),1,1).is_none());let e=PreparedSymbolEpoch::capture(inputs(),Some(&p),123).unwrap();assert!(e.bytes()<=KEY_BUDGET);assert_eq!(e.pattern_payload_bytes,123);assert_eq!(p.colors.len(),1);}
 #[test]fn real_context_mutations_invalidate_but_sorted_view_changes_do_not(){
  use ferrite_render::{RenderContext,Viewport,DrawingInstruction,LineInstruction,WorldPoint};
  let mut c=RenderContext::new(Viewport::new(960.,640.));
  c.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![WorldPoint::new(0.,0.),WorldPoint::new(1.,1.)])));
  c.get_sorted_instructions();let key=Inputs{source_epoch:c.geometry_revision(),instruction_count:c.instruction_count(),..inputs()};let e=PreparedSymbolEpoch::capture(key,None,0).unwrap();
  c.set_viewport(640.,480.);c.get_sorted_instructions();assert!(e.matches(Inputs{source_epoch:c.geometry_revision(),instruction_count:c.instruction_count(),..key},None));
  let original=c.raw_instructions().to_vec();c.set_instructions_from_cache(original);assert!(!e.matches(Inputs{source_epoch:c.geometry_revision(),instruction_count:c.instruction_count(),..key},None));
  c.truncate_instructions(0);assert!(!e.matches(Inputs{source_epoch:c.geometry_revision(),instruction_count:c.instruction_count(),..key},None));
  c.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![WorldPoint::new(2.,2.),WorldPoint::new(3.,3.)])));assert!(!e.matches(Inputs{source_epoch:c.geometry_revision(),instruction_count:c.instruction_count(),..key},None));
  let other=RenderContext::new(Viewport::new(640.,480.));assert!(!e.matches(Inputs{source_epoch:other.geometry_revision(),instruction_count:other.instruction_count(),..key},None));
 }

}
