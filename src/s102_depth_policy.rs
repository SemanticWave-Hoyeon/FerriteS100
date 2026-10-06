//! Host-supplied constant corrections, scoped to one exact dataset content digest.
//! This configuration is application input, not dataset-authenticated transformation evidence.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::depth_selection::{DepthAdjustment, DepthAdjustmentProvider, DepthReference};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    datasets: Vec<Dataset>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dataset {
    sha256: String,
    target_datum: u32,
    corrections: Vec<Correction>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Correction {
    datum: u32,
    correction_metres: f64,
    source: String,
}
#[derive(Clone)]
pub struct DepthPolicy {
    pub target: u32,
    corrections: Vec<(u32, f64, String)>,
}
fn datum(value: u32) -> Result<()> {
    ensure!(
        (1..=30).contains(&value) || value == 44,
        "Unsupported vertical datum classification"
    );
    Ok(())
}
impl DepthPolicy {
    pub fn load(config: Option<&Path>, data: &Path, datums: &[u32]) -> Result<Self> {
        let default = *datums.first().context("No S102 instances")?;
        let Some(config) = config else {
            return Ok(Self {
                target: default,
                corrections: Vec::new(),
            });
        };
        let mut bytes = Vec::new();
        std::fs::File::open(config)
            .context("Read S102 depth adjustment configuration")?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "S102 adjustment configuration exceeds 1MiB"
        );
        let configuration: Configuration = serde_json::from_slice(&bytes)?;
        let mut digests = std::collections::HashSet::new();
        for d in &configuration.datasets {
            ensure!(
                d.sha256.len() == 64 && d.sha256.bytes().all(|c| c.is_ascii_hexdigit()),
                "Invalid dataset SHA256"
            );
            ensure!(
                digests.insert(d.sha256.to_ascii_lowercase()),
                "Duplicate dataset adjustment SHA256"
            );
        }
        let mut file = std::fs::File::open(data)?;
        let mut hash = Sha256::new();
        let mut block = [0u8; 65536];
        loop {
            let n = file.read(&mut block)?;
            if n == 0 {
                break;
            }
            hash.update(&block[..n]);
        }
        let digest = format!("{:x}", hash.finalize());
        let Some(d) = configuration
            .datasets
            .into_iter()
            .find(|d| d.sha256.eq_ignore_ascii_case(&digest))
        else {
            return Ok(Self {
                target: default,
                corrections: Vec::new(),
            });
        };
        datum(d.target_datum)?;
        let mut seen = std::collections::HashSet::new();
        let mut corrections = Vec::new();
        for c in d.corrections {
            datum(c.datum)?;
            ensure!(
                datums.contains(&c.datum),
                "Correction references absent source datum"
            );
            ensure!(
                c.datum != d.target_datum,
                "Identity datum correction must not be overridden"
            );
            ensure!(seen.insert(c.datum), "Duplicate source datum correction");
            ensure!(
                c.correction_metres.is_finite() && !c.source.trim().is_empty(),
                "Invalid correction or missing provenance source"
            );
            corrections.push((c.datum, c.correction_metres, c.source));
        }
        Ok(Self {
            target: d.target_datum,
            corrections,
        })
    }
    pub fn identity(&self, source: u32) -> bool {
        self.target == source && self.corrections.is_empty()
    }
    pub fn source(&self, id: u64) -> &str {
        if id == 0 {
            "Scoped identity (same dataset datum)"
        } else {
            self.corrections
                .get((id - 1) as usize)
                .map(|c| c.2.as_str())
                .unwrap_or("Unknown correction provenance")
        }
    }
}
impl DepthAdjustmentProvider for DepthPolicy {
    fn adjustment(
        &self,
        from: DepthReference,
        to: DepthReference,
        _: f64,
        _: f64,
    ) -> Result<Option<DepthAdjustment>> {
        ensure!(
            to == DepthReference(self.target as u64),
            "Depth policy target mismatch"
        );
        if from == to {
            return Ok(Some(DepthAdjustment {
                correction_metres: 0.,
                provenance: 0,
            }));
        }
        Ok(self
            .corrections
            .iter()
            .enumerate()
            .find(|(_, c)| c.0 as u64 == from.0)
            .map(|(index, c)| DepthAdjustment {
                correction_metres: c.1,
                provenance: index as u64 + 1,
            }))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrections_bound_to_digest_and_provenance_and_constant_target() {
        let dir = std::env::temp_dir().join(format!("ferrite-depth-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("dataset");
        let cfg = dir.join("policy.json");
        std::fs::write(&data, b"fixture").unwrap();
        let sha = format!("{:x}", Sha256::digest(b"fixture"));
        let document = serde_json::json!({"datasets":[{"sha256":sha,"target_datum":10,"corrections":[{"datum":23,"correction_metres":-4.,"source":"Synthetic unit fixture; not navigational evidence"}]}]});
        std::fs::write(&cfg, serde_json::to_vec(&document).unwrap()).unwrap();
        let p = DepthPolicy::load(Some(&cfg), &data, &[10, 23]).unwrap();
        let a = p
            .adjustment(DepthReference(23), DepthReference(10), 1., 2.)
            .unwrap()
            .unwrap();
        assert_eq!(a.correction_metres, -4.);
        assert!(p.source(a.provenance).contains("Synthetic"));
        assert!(p
            .adjustment(DepthReference(44), DepthReference(10), 1., 2.)
            .unwrap()
            .is_none());
        std::fs::write(&data, b"changed").unwrap();
        let p = DepthPolicy::load(Some(&cfg), &data, &[23]).unwrap();
        assert_eq!(p.target, 23);
        assert!(p.identity(23));
        for corrections in [
            serde_json::json!([{"datum":10,"correction_metres":1.,"source":"bad"}]),
            serde_json::json!([{"datum":23,"correction_metres":1.,"source":""}]),
            serde_json::json!([{"datum":23,"correction_metres":1.,"source":"a"},{"datum":23,"correction_metres":2.,"source":"b"}]),
        ] {
            std::fs::write(&data, b"fixture").unwrap();
            let mut d = document.clone();
            d["datasets"][0]["corrections"] = corrections;
            std::fs::write(&cfg, serde_json::to_vec(&d).unwrap()).unwrap();
            assert!(DepthPolicy::load(Some(&cfg), &data, &[10, 23]).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
