use ferrite_s102::{hdf5, BathymetryCoverage};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut rows = Vec::new();
    for path in std::env::args().skip(1) {
        let file = hdf5::File::open(&path)?;
        let group = file.group("BathymetryCoverage")?;
        let coverages = BathymetryCoverage::open(&path)?;
        rows.push(serde_json::json!({"path":path,"dimension":group.attr("dimension")?.read_scalar::<u8>()?,"commonPointRule":group.attr("commonPointRule")?.read_scalar::<u8>()?,"interpolationType":group.attr("interpolationType")?.read_scalar::<u8>()?,"numInstances":group.attr("numInstances")?.read_scalar::<u8>()?,"verticalCS":file.attr("verticalCS")?.read_scalar::<u32>()?,"verticalCoordinateBase":file.attr("verticalCoordinateBase")?.read_scalar::<u8>()?,"rootDatum":file.attr("verticalDatum")?.read_scalar::<u32>()?,"instances":coverages.iter().map(|c|serde_json::json!({"name":c.instance_name,"effectiveDatum":c.vertical_datum,"datumReference":c.vertical_datum_reference,"uncertaintyEncoding":format!("{:?}",c.uncertainty_encoding),"minimumUncertainty":c.declared_min_uncertainty,"maximumUncertainty":c.declared_max_uncertainty,"timePoint":c.time_point})).collect::<Vec<_>>() }));
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}
