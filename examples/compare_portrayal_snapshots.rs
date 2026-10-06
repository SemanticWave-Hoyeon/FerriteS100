//! Inspect the serialized output captured by the opt-in native IC audit.
use anyhow::{Context, Result};
use ferrite_render::DrawingInstruction;
fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let read = |path: &str| -> Result<Vec<DrawingInstruction>> {
        Ok(bincode::deserialize(&std::fs::read(path)?)?)
    };
    let a = read(args.get(1).context("first snapshot")?)?;
    let b = read(args.get(2).context("second snapshot")?)?;
    let mut differences = Vec::new();
    let mut changed = 0;
    for index in 0..a.len().max(b.len()) {
        let left = a.get(index).map(serde_json::to_value).transpose()?;
        let right = b.get(index).map(serde_json::to_value).transpose()?;
        if left != right {
            changed += 1;
            if differences.len() < 40 {
                differences.push(serde_json::json!({"index":index,"left":left,"right":right}));
            }
        }
    }
    let report = serde_json::json!({"first_count":a.len(),"second_count":b.len(),"changed_indices":changed,"first_40_differences":differences});
    std::fs::write(
        args.get(3).context("output JSON")?,
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!(
        "{} / {} instructions; {} changed indices",
        a.len(),
        b.len(),
        changed
    );
    Ok(())
}
