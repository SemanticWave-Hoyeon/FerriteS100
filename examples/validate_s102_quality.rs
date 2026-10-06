use anyhow::{ensure, Result};
use ferrite_kernel::{CoverageSource, GridWindow};
use ferrite_s102::BathymetryCoverage;
fn main() -> Result<()> {
    let root = std::env::args().nth(1).expect("S-102 directory");
    let mut paths: Vec<_> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && !e.file_name().as_encoded_bytes().starts_with(b"._")
                && e.path()
                    .extension()
                    .is_some_and(|s| s.eq_ignore_ascii_case("h5"))
        })
        .map(|e| e.into_path())
        .collect();
    paths.sort();
    let mut output = Vec::new();
    for path in paths {
        for c in BathymetryCoverage::open(&path)? {
            let g = c.geometry();
            let quality = c
                .quality
                .as_ref()
                .expect("Expected quality coverage in this test set");
            let mut counts = std::collections::BTreeMap::new();
            let mut sample = None;
            for row in (0..g.height).step_by(128) {
                let w = GridWindow {
                    column: 0,
                    row,
                    width: g.width,
                    height: 128.min(g.height - row),
                };
                let ids = quality.read_window_ids(w)?;
                for (index, id) in ids.into_iter().enumerate() {
                    *counts.entry(id).or_insert(0usize) += 1;
                    if sample.is_none() && id != 0 {
                        let (x, y) = g.position(index % g.width, row + index / g.width).unwrap();
                        let record = quality.sample_nearest(x, y)?.unwrap();
                        ensure!(record.id == id, "Quality point/grid mismatch");
                        sample = Some(
                            serde_json::json!({"position":[x,y],"id":id,"details":record.description()}),
                        );
                    }
                }
            }
            let mut records: Vec<_> = quality.records().collect();
            records.sort_by_key(|r| r.id);
            let rows:Vec<_>=records.iter().map(|r|serde_json::json!({"id":r.id,"rawStringBytes":r.raw_string_bytes,"encodingWarnings":r.encoding_warnings,"dataAssessment":r.data_assessment,"leastDepthCapability":r.least_depth_measurement_capability,"significantFeatureCapability":r.significant_feature_detection_capability,"featureSize":r.size_of_features_detected,"featureSizeVar":r.feature_size_variation,"fullSeafloorCoverage":r.full_seafloor_coverage,"bathyCoverage":r.bathymetry_observed,"horizontalFixed":r.horizontal_uncertainty_fixed,"horizontalVariableFactor":r.horizontal_uncertainty_variable_factor,"surveyStart":r.survey_date_start,"surveyEnd":r.survey_date_end,"sourceSurveyID":r.source_survey_id,"surveyAuthority":r.survey_authority,"uncertaintyType":r.uncertainty_type})).collect();
            output.push(serde_json::json!({"file":path,"instance":c.instance_name,"shape":[g.height,g.width],"idCounts":counts,"records":rows,"pointProbe":sample,"maximum_window_rows":128}));
        }
    }
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
