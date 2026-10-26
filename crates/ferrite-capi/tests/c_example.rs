//! Build `examples/c/render.c` with the system C compiler against the
//! library's header and shared library, and run it on the SHOM exchange set
//! (skipped without a C compiler or the test data).
use std::path::{Path, PathBuf};
use std::process::Command;

const EXCHANGE_SET: &str =
    "TestData/SHOM-S101/ExchangeSets/S100_ENC_ROOT_AcrossTheCHannel_product_Edition1_2026-03-25T06_53_12";

/// The directory holding the cdylib built for these tests (`target/<profile>`).
fn library_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_path_buf()
}

fn library_file(dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        dir.join("libferrite.dylib")
    } else if cfg!(windows) {
        dir.join("ferrite.dll")
    } else {
        dir.join("libferrite.so")
    }
}

#[test]
fn c_example_builds_and_renders() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.join("../..");
    if cfg!(windows) || Command::new("cc").arg("--version").output().is_err() {
        eprintln!("no C compiler; skipped");
        return;
    }
    if !root.join(EXCHANGE_SET).is_dir() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let lib_dir = library_dir();
    assert!(
        library_file(&lib_dir).is_file(),
        "{} not built",
        library_file(&lib_dir).display()
    );
    let work = std::env::temp_dir().join(format!("ferrite-capi-c-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let exe = work.join("render");
    let status = Command::new("cc")
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(crate_dir.join("include"))
        .arg(crate_dir.join("examples/c/render.c"))
        .arg("-L")
        .arg(&lib_dir)
        .args(["-lferrite", "-o"])
        .arg(&exe)
        .status()
        .unwrap();
    assert!(status.success(), "C example did not compile");
    let output = Command::new(&exe)
        .arg(root.join("Catalogues"))
        .arg(root.join(EXCHANGE_SET))
        .arg(work.join("chart.ppm"))
        .args(["49.21", "-2.13", "20"])
        .env("DYLD_LIBRARY_PATH", &lib_dir)
        .env("LD_LIBRARY_PATH", &lib_dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_dir_all(&work);
    if !output.status.success() && stderr.contains("GPU") {
        eprintln!("no GPU adapter; render skipped: {stderr}");
        return;
    }
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("datasets: "), "{stdout}");
    assert!(stdout.contains("pick: [{"), "{stdout}");
    assert!(stdout.trim_end().ends_with("ok"), "{stdout}");
}
