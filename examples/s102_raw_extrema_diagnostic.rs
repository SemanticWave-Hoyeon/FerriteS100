//! Raw HDF5 evidence only; does not use the adapter's decoding/range validation.
#![allow(non_local_definitions)]
use ferrite_s102::hdf5;
use hdf5::H5Type;
#[derive(H5Type,Clone,Copy)] #[repr(C)] struct Value{depth:f32,uncertainty:f32}
fn main()->anyhow::Result<()> {
 let mut rows=Vec::new();
 for entry in walkdir::WalkDir::new(std::env::args().nth(1).unwrap()) {
  let entry=entry?;let p=entry.path();if !entry.file_type().is_file()||!p.extension().is_some_and(|x|x.eq_ignore_ascii_case("h5"))||p.file_name().unwrap().to_string_lossy().starts_with("._") {continue;}
  let f=hdf5::File::open(p)?;let b=f.group("BathymetryCoverage")?;
  for name in b.member_names()? {if !name.starts_with("BathymetryCoverage."){continue;}
   let g=b.group(&name)?.group("Group_001")?;let values=g.dataset("values")?;let shape=values.shape();
   let bounds=[g.attr("minimumDepth")?.read_scalar::<f32>()?,g.attr("maximumDepth")?.read_scalar::<f32>()?,g.attr("minimumUncertainty")?.read_scalar::<f32>()?,g.attr("maximumUncertainty")?.read_scalar::<f32>()?];
   let mut observed=[f32::INFINITY,f32::NEG_INFINITY,f32::INFINITY,f32::NEG_INFINITY];let mut bad=[0usize;2];let mut first=Vec::new();
   for r in (0..shape[0]).step_by(128) {let tile=values.read_slice_2d::<Value,_>((r..(r+128).min(shape[0]),0..shape[1]))?;
    for (i,v) in tile.iter().enumerate() {for (k,x) in [(0,v.depth),(1,v.uncertainty)] {if x==1e6 {continue;}
     observed[2*k]=observed[2*k].min(x);observed[2*k+1]=observed[2*k+1].max(x);
     if !x.is_finite()||x<bounds[2*k]||x>bounds[2*k+1] {bad[k]+=1;if first.len()<8 {first.push(serde_json::json!({"column":i%shape[1],"row":r+i/shape[1],"field":k,"value":x,"depth":v.depth,"uncertainty":v.uncertainty}));}}
    }}
   }
   rows.push(serde_json::json!({"path":p,"instance":name,"shape":shape,"bounds":bounds,"observed":observed,"out_of_bounds":bad,"first":first}));
  }
 }
 println!("{}",serde_json::to_string_pretty(&rows)?);Ok(())
}
