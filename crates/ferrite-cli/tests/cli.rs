//! The `ferrite` binary on the SHOM test data: inspection and validation as
//! JSON, rendering a PNG, and precomputed portrayal reproducing the Lua image.
use std::path::PathBuf;
use std::process::Command;

const EXCHANGE_SET: &str =
    "TestData/SHOM-S101/ExchangeSets/S100_ENC_ROOT_AcrossTheCHannel_product_Edition1_2026-03-25T06_53_12";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ferrite(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .current_dir(root())
        .args(args)
        .output()
        .unwrap()
}

fn work_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrite-cli-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn usage_and_errors_exit_with_status_two() {
    let output = ferrite(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("USAGE"));
    let output = ferrite(&["render", "--zoom"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--zoom needs a value"));
}

#[test]
fn inspects_validates_and_renders() {
    if !root().join(EXCHANGE_SET).is_dir() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let output = ferrite(&["inspect", "--json", EXCHANGE_SET]);
    assert!(output.status.success(), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let datasets = json["datasets"].as_array().unwrap();
    assert!(!datasets.is_empty());
    assert!(datasets.iter().all(|d| d["product"] == "S-101"));
    assert!(datasets[0]["feature_types"].as_object().unwrap().len() > 5);

    let output = ferrite(&["validate", "--json", EXCHANGE_SET]);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        output.status.code(),
        Some(if json["valid"] == true { 0 } else { 1 })
    );
    assert_eq!(json["datasets"], datasets.len());

    let dir = work_dir("render");
    let lua = dir.join("lua.png");
    let view = [
        "--center",
        "49.21,-2.13",
        "--zoom",
        "20",
        "--size",
        "320x200",
    ];
    let mut args = vec!["render", EXCHANGE_SET, "--out", lua.to_str().unwrap()];
    args.extend(view);
    let output = ferrite(&args);
    if !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("GPU") {
        eprintln!("no GPU adapter; render skipped");
        return;
    }
    assert!(output.status.success(), "{output:?}");
    let image = image::open(&lua).unwrap().to_rgba8();
    assert_eq!(image.dimensions(), (320, 200));
    assert!(image.pixels().any(|p| p != image.get_pixel(0, 0)));

    // Precompute the payloads of this view, then render from them alone.
    let payloads = dir.join("payloads");
    let mut args = vec![
        "precompute",
        EXCHANGE_SET,
        "--cache-dir",
        payloads.to_str().unwrap(),
    ];
    args.extend(view);
    let output = ferrite(&args);
    assert!(output.status.success(), "{output:?}");
    let precomputed = dir.join("precomputed.png");
    let mut args = vec![
        "render",
        EXCHANGE_SET,
        "--precomputed",
        payloads.to_str().unwrap(),
        "--out",
        precomputed.to_str().unwrap(),
    ];
    args.extend(view);
    let output = ferrite(&args);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        image::open(&precomputed).unwrap().to_rgba8().as_raw(),
        image.as_raw(),
        "precomputed portrayal draws the same image"
    );
    // Another palette has no payloads: a clear error.
    let mut args = vec![
        "render",
        EXCHANGE_SET,
        "--precomputed",
        payloads.to_str().unwrap(),
        "--palette",
        "Night",
        "--out",
        precomputed.to_str().unwrap(),
    ];
    args.extend(view);
    let output = ferrite(&args);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("No precomputed portrayal"));
    let _ = std::fs::remove_dir_all(&dir);
}
