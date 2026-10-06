//! Metadata-only independent admission audit; no raster flattening or full-grid reads.
use anyhow::{Context,Result};
use ferrite_kernel::CoverageSource;
use ferrite_s102::{BathymetryCoverage,InstanceDomain};
fn kind(d:&InstanceDomain)-> &'static str {match d {InstanceDomain::FullGrid=>"full-grid",InstanceDomain::Rectangle(_)=>"rectangle",InstanceDomain::Polygon(_)=>"polygon"}}
fn main()->Result<()> {
 let root=std::env::args().nth(1).context("usage: audit_s102_domains S102_DIRECTORY")?;
 let mut rows=Vec::new();
 for entry in walkdir::WalkDir::new(root) {
  let e=entry?;if !e.file_type().is_file() || e.file_name().to_string_lossy().starts_with("._") || !e.path().extension().is_some_and(|s|s.eq_ignore_ascii_case("h5")) {continue;}
  for c in BathymetryCoverage::open(e.path())? {
   rows.push(serde_json::json!({"file":e.path(),"instance":c.instance_name,"declared_features":c.feature_metadata.declared_features,"feature_metadata_diagnostics":format!("{:?}",c.feature_metadata.diagnostics),"feature_declaration_valid":c.feature_metadata.validate_declaration().is_ok(),"feature_definitions":c.feature_metadata.bathymetry.iter().chain(c.feature_metadata.quality.iter()).map(|r|serde_json::json!({"code":r.code,"name":r.name,"uom.name":r.unit,"fillValue":r.fill_value,"datatype":r.datatype,"lower":r.lower,"upper":r.upper,"closure":r.closure})).collect::<Vec<_>>(),"issue_date":c.issue.date,"issue_time":c.issue.time,"axis_names":c.axes.names,"axis_names_array_major_order":c.axes.matches_generic_array_major_order(),"quality_axis_names":c.quality.as_ref().map(|q|&q.axes.names),"quality_axis_names_array_major_order":c.quality.as_ref().map(|q|q.axes.matches_generic_array_major_order()),"domain":kind(&c.domain),"horizontal_crs":c.geometry().horizontal_crs,"requires_geometric_mask":c.requires_geometric_mask(),"root_geographic_bounds":c.root_bounds.encoded,"root_full_grid_enclosure":format!("{:?}",c.root_enclosure.full_grid),"root_declared_domain_enclosure":format!("{:?}",c.root_enclosure.declared_domain),"quality_domain":c.quality.as_ref().map(|q|kind(&q.domain)),"quality_root_enclosure":c.quality.as_ref().map(|q|format!("{:?}",q.root_enclosure))}));
  }
 }
 anyhow::ensure!(!rows.is_empty(),"No S102 instances found");
 println!("{}",serde_json::to_string_pretty(&rows)?);Ok(())
}
