//! Native half of the Lua-on-web parity check (`tools/lua_wasm_parity.py`):
//! the raw Part 9a output of the S-101 Lua rules for groups of datasets,
//! through the same `WebEngineCore` the browser build uses.
//!
//!     part9a_dump MANIFEST.json OUT.txt
//!
//! The manifest is `{"catalogues": [[name, path]...], "groups": [{"name":
//! ..., "files": [[name, path]...]}...]}`; every group is opened in a fresh
//! engine. The wasm half (`tools/lua_wasm_parity.mjs`) writes the same text.
use anyhow::{Context, Result};
use ferrite_engine::platform::SystemClock;
use ferrite_wasm::core::WebEngineCore;
use std::sync::Arc;

fn read_pairs(value: &serde_json::Value) -> Result<Vec<(String, Vec<u8>)>> {
    value
        .as_array()
        .context("Expected [[name, path]...]")?
        .iter()
        .map(|pair| {
            let name = pair[0].as_str().context("name")?.to_owned();
            let path = pair[1].as_str().context("path")?;
            Ok((
                name,
                std::fs::read(path).with_context(|| format!("Read {path}"))?,
            ))
        })
        .collect()
}

/// `open\t<datasets>\t<issue>`: what the open found (both halves alike).
fn open_line(report: &serde_json::Value) -> String {
    let datasets = report["datasets"].as_array().map_or(0, Vec::len);
    let issue = report["issues"]
        .as_array()
        .and_then(|issues| issues.iter().find(|i| i["severity"] == "error"))
        .map(|i| {
            format!(
                "{}: {}",
                i["check"].as_str().unwrap_or(""),
                i["message"].as_str().unwrap_or("")
            )
        })
        .unwrap_or_default();
    format!("open\t{datasets}\t{issue}\n")
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 2, "usage: part9a_dump MANIFEST.json OUT.txt");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let catalogues = read_pairs(&manifest["catalogues"])?;
    let mut out = String::new();
    for group in manifest["groups"].as_array().context("groups")? {
        let name = group["name"].as_str().context("group name")?;
        let started = std::time::Instant::now();
        out.push_str(&format!("group\t{name}\n"));
        let result = (|| -> Result<String> {
            let mut core = WebEngineCore::new(catalogues.clone(), Arc::new(SystemClock))?;
            for (file, bytes) in read_pairs(&group["files"])? {
                core.add_file(&file, bytes)?;
            }
            let report: serde_json::Value = serde_json::from_str(&core.open()?)?;
            Ok(open_line(&report) + &core.part9a_dump()?)
        })();
        match result {
            Ok(dump) => out.push_str(&dump),
            Err(error) => out.push_str(&format!("error\t{error:#}\n")),
        }
        eprintln!("{name}: {:.1} s", started.elapsed().as_secs_f64());
    }
    std::fs::write(&args[1], out)?;
    Ok(())
}
