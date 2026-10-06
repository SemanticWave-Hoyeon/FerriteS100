//! Offline S-100 Part 15 authentication. Arguments: S100_ROOT IHO.PEM [Unix verification time].
use anyhow::{Context, Result};
use ferrite_security::{verify_exchange, TrustAnchors};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let root = args.get(1).context("Pass exchange set root")?;
    let pem = args
        .get(2)
        .context("Pass independently installed IHO trust anchor")?;
    let time = if let Some(t) = args.get(3) {
        t.parse()?
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64
    };
    let mut anchors = TrustAnchors::default();
    anchors.install_pem("IHO", &std::fs::read(pem)?)?;
    let report = verify_exchange(root, &anchors, time)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
