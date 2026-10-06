//! Bounded raw-source declaration/range audit. No window, device or sample repair.
//! --require-clean covers these named checks only, not full S102 certification.
//! Bits are decoded host values after HDF conversion, not guaranteed storage bits.
//! Group-constant uncertainty has no encoded uncertainty definition in this profile.
use anyhow::{ensure,Context,Result};
use ferrite_kernel::CoverageSource;
use ferrite_s102::{BathymetryCoverage,FeatureDefinition,UncertaintyEncoding,hdf5};
use hdf5::{H5Type,types::{CompoundField,CompoundType,TypeDescriptor}};
const COLS:usize=2048;const ROWS:usize=128;const EXAMPLES:usize=8;
#[repr(C)]struct Column<const N:usize>{value:f32}
// SAFETY: named sole repr(C) f32 field at offset0, exact size and HDF type.
unsafe impl<const N:usize> H5Type for Column<N> {
 fn type_descriptor()->TypeDescriptor {TypeDescriptor::Compound(CompoundType{fields:vec![CompoundField{name:if N==0{"depth"}else{"uncertainty"}.into(),ty:f32::type_descriptor(),offset:0,index:0}],size:std::mem::size_of::<Self>()})}
}
#[derive(Default)]struct Stats {
 total:u64,fill:u64,finite:u64,nonfinite:u64,outside_interval:u64,outside_group_extrema:u64,orphan_id:u64,
 min:Option<f64>,max:Option<f64>,examples:Vec<serde_json::Value>,windows:u64,
}
impl Stats {
 fn observe(&mut self,v:f64,bits:u32,fill:bool,definition:&FeatureDefinition,extrema:Option<[f32;2]>,orphan:bool,row:usize,col:usize) {
  self.total+=1;if fill {self.fill+=1;return;}
  let mut reasons=0u8;
  if !v.is_finite(){self.nonfinite+=1;reasons|=1;}else{
   self.finite+=1;self.min=Some(self.min.map_or(v,|n|n.min(v)));self.max=Some(self.max.map_or(v,|n|n.max(v)));
   if !definition.interval.contains(v){self.outside_interval+=1;reasons|=2;}
   if extrema.is_some_and(|e|v<f64::from(e[0])||v>f64::from(e[1])) {self.outside_group_extrema+=1;reasons|=4;}
  }
  if orphan {self.orphan_id+=1;reasons|=8;}
  if reasons!=0 && self.examples.len()<EXAMPLES {
   let reasons:Vec<_>=[(1,"Nonfinite"),(2,"OutsideFeatureDefinitionInterval"),(4,"OutsideGroupExtrema"),(8,"MissingQualityAttributeRecord")].into_iter().filter_map(|(bit,name)|(reasons&bit!=0).then_some(name)).collect();
   self.examples.push(serde_json::json!({"row":row,"column":col,"decoded32_bits":format!("{bits:08x}"),"finite_value":v.is_finite().then_some(v),"reasons":reasons}));
  }
 }
 fn clean(&self)->bool {self.nonfinite==0 && self.outside_interval==0 && self.outside_group_extrema==0 && self.orphan_id==0}
 fn json(&self,code:&str,kind:&str)->serde_json::Value {serde_json::json!({"code":code,"raw_datatype":kind,"total":self.total,"fill":self.fill,"finite":self.finite,"nonfinite":self.nonfinite,"outside_feature_interval":self.outside_interval,"outside_group_extrema":self.outside_group_extrema,"orphan_quality_id":self.orphan_id,"min":self.min,"max":self.max,"examples":self.examples,"read_windows":self.windows,"clean":self.clean()})}
}
fn scan_float<const N:usize>(d:&hdf5::Dataset,def:&FeatureDefinition,extrema:[f32;2])->Result<Stats> {
 let shape=d.shape();ensure!(shape.len()==2,"Raw sample array must be2D");
 let TypeDescriptor::Compound(kind)=d.dtype()?.to_descriptor()? else {anyhow::bail!("Expected raw compound float array")};
 let field=if N==0 {"depth"}else{"uncertainty"};ensure!(kind.fields.iter().any(|f|f.name==field && f.ty==TypeDescriptor::Float(hdf5::types::FloatSize::U4)),"Raw field is not float32");let mut stats=Stats::default();
 for row in (0..shape[0]).step_by(ROWS) {for col in (0..shape[1]).step_by(COLS) {
  let tile=d.read_slice_2d::<Column<N>,_>((row..(row+ROWS).min(shape[0]),col..(col+COLS).min(shape[1])))?;
  stats.windows+=1;let width=tile.shape()[1];
  for (i,x) in tile.iter().enumerate() {stats.observe(f64::from(x.value),x.value.to_bits(),x.value==1e6,def,Some(extrema),false,row+i/width,col+i%width);}
 }}
 Ok(stats)
}
fn paths(root:&std::path::Path)->Result<Vec<std::path::PathBuf>> {
 let mut paths=Vec::new();for entry in walkdir::WalkDir::new(root) {
  let e=entry?;if e.file_type().is_file() && !e.file_name().to_string_lossy().starts_with("._") && e.path().extension().is_some_and(|s|s.eq_ignore_ascii_case("h5")){paths.push(e.into_path());}
 }ensure!(!paths.is_empty(),"No S102 HDF5 inputs found");paths.sort();Ok(paths)
}
fn main()->Result<()> {
 let mut args=std::env::args_os().skip(1);let root=args.next().context("usage: audit_s102_feature_ranges DIRECTORY [--require-clean]")?;
 let option=args.next();ensure!(option.as_deref().is_none_or(|s|s=="--require-clean"),"Unknown option");ensure!(args.next().is_none(),"Extra arguments");let strict=option.is_some();let files=paths(std::path::Path::new(&root))?;let mut result=Vec::new();let mut clean=true;
 for path in files {
  let coverages=BathymetryCoverage::open(&path)?;let file=hdf5::File::open(&path)?;
  for c in &coverages {
   let metadata=&c.feature_metadata;let declaration_clean=metadata.validate_declaration().is_ok();clean&=declaration_clean;
   let values=file.dataset(&format!("BathymetryCoverage/{}/Group_001/values",c.instance_name))?;ensure!(values.shape()==[c.geometry().height,c.geometry().width],"Source shape changed during audit");
   let depth=metadata.bathymetry.iter().find(|r|r.code=="depth").unwrap();let depthstats=scan_float::<0>(&values,depth,[c.declared_min_depth,c.declared_max_depth])?;clean&=depthstats.clean();
   let uncertainty=if c.uncertainty_encoding==UncertaintyEncoding::PerCell {
    let def=metadata.bathymetry.iter().find(|r|r.code=="uncertainty").context("Missing encoded uncertainty definition")?;
    let stats=scan_float::<1>(&values,def,[c.declared_min_uncertainty,c.declared_max_uncertainty])?;clean&=stats.clean();Some(stats.json("uncertainty","float32"))
   }else{None};
   result.push(serde_json::json!({"file":path,"instance":c.instance_name,"declared_features":metadata.declared_features,"feature_declaration_clean":declaration_clean,"declaration_diagnostics":format!("{:?}",metadata.diagnostics),"depth":depthstats.json("depth","float32"),"encoded_cell_uncertainty":uncertainty,"group_constant_uncertainty":(c.uncertainty_encoding==UncertaintyEncoding::GroupConstant).then_some(c.declared_min_uncertainty),"scope":"encoded feature declaration and raw value interval/group-extrema checks; no fullproducer certificate"}));
  }
  if let Some(q)=coverages[0].quality.as_ref() {
   let g=file.group("QualityOfBathymetryCoverage")?;let names:Vec<_>=g.member_names()?.into_iter().filter(|n|n.starts_with("QualityOfBathymetryCoverage.")).collect();ensure!(names.len()==1,"Quality instance count changed during audit");
   let d=g.dataset(&format!("{}/Group_001/values",names[0]))?;let shape=d.shape();ensure!(shape==[q.geometry().height,q.geometry().width],"Quality shape changed during audit");
   ensure!(d.dtype()?.to_descriptor()?==TypeDescriptor::Unsigned(hdf5::types::IntSize::U4),"Raw quality ID type is not uint32");
   let definition=&coverages[0].feature_metadata.quality[0];let known:std::collections::HashSet<u32>=q.records().map(|r|r.id).collect();let mut stats=Stats::default();
   for row in (0..shape[0]).step_by(ROWS) {for col in (0..shape[1]).step_by(COLS) {
    let tile=d.read_slice_2d::<u32,_>((row..(row+ROWS).min(shape[0]),col..(col+COLS).min(shape[1])))?;stats.windows+=1;let width=tile.shape()[1];
    for (i,id) in tile.iter().enumerate(){stats.observe(f64::from(*id),*id,*id==0,definition,None,*id!=0&&!known.contains(id),row+i/width,col+i%width);}
   }}clean&=stats.clean();result.push(serde_json::json!({"file":path,"instance":names[0],"quality_id":stats.json("iD","uint32")}));
  }
 }
 println!("{}",serde_json::to_string_pretty(&serde_json::json!({"rows":result,"clean_for_named_checks":clean,"maximum_read_window":[ROWS,COLS],"maximum_examples_per_field":EXAMPLES,"required_metadata_loaded_by_receiver":true,"raw_values_repaired":false,"bit_provenance":"decoded host values after HDF type conversion; input file bits require separate storage/hash evidence","constant_uncertainty_interval_checked":false,"sample_membership_domain_checked":false,"group_extrema_equality_checked":false,"input_identity":"external before/after SHA receipts required; this standalone tool does not hash input","fullproducerconformance":false}))?);
 ensure!(!strict || clean,"Declared feature list or raw ranges failed named audit checks; see JSON report");Ok(())
}
#[cfg(test)]mod tests {
 use super::*;use ferrite_kernel::ExactDecimal;use ferrite_s102::{DefinitionInterval,IntervalClosure};
 fn definition()->FeatureDefinition {FeatureDefinition{code:"depth".into(),name:"depth".into(),unit:"metres".into(),fill_value:"1000000".into(),datatype:"H5T_FLOAT".into(),lower:"-14".into(),upper:"11050".into(),closure:"closedInterval".into(),interval:DefinitionInterval{lower:Some(ExactDecimal::parse("-14").unwrap()),upper:Some(ExactDecimal::parse("11050").unwrap()),closure:IntervalClosure::Closed}}}
 #[test]fn decoded_bits_nonfinite_fill_and_independent_group_bounds_are_reported_without_repair() {
  let d=definition();let mut s=Stats::default();let values=[-14f32,11050.,(-14f32).next_down(),11050f32.next_up(),1e6,f32::from_bits(0x7fc12345),f32::INFINITY,0.];
  for (i,v) in values.into_iter().enumerate(){s.observe(f64::from(v),v.to_bits(),v==1e6,&d,Some([-14.,11050.]),false,0,i);}
  assert_eq!((s.total,s.fill,s.finite,s.nonfinite,s.outside_interval,s.outside_group_extrema),(8,1,5,2,2,2));assert_eq!(s.examples[2]["decoded32_bits"],"7fc12345");assert!(!s.clean());
  for i in 0..100{s.observe(-100.,(-100f32).to_bits(),false,&d,None,false,1,i);}assert_eq!(s.examples.len(),EXAMPLES);
 }
 #[test]fn all_nodata_and_empty_directory_are_not_confused() {
  let d=definition();let mut s=Stats::default();for i in 0..3{s.observe(1e6,(1e6f32).to_bits(),true,&d,Some([1e6,1e6]),false,0,i);}assert!(s.clean());assert_eq!(s.fill,3);assert!(s.min.is_none());
  let p=std::env::temp_dir().join(format!("s102-range-empty-{}",std::process::id()));std::fs::create_dir_all(&p).unwrap();assert!(paths(&p).is_err());assert!(paths(&p.join("missing")).is_err());std::fs::remove_dir(&p).unwrap();
 }
}
