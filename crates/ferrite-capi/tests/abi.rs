//! The C entry points called as C would: argument errors, missing scenes and
//! failures are status codes with a message, never panics or crashes; an
//! in-memory cell opens, renders and picks.
use ferrite::*;
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::ptr::{null, null_mut};

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(ferrite_last_error()) }
        .to_string_lossy()
        .into_owned()
}

#[test]
fn invalid_arguments_are_reported() {
    unsafe {
        assert!(!CStr::from_ptr(ferrite_version()).to_bytes().is_empty());
        assert_eq!(
            ferrite_engine_open_folder(null_mut(), null()),
            FERRITE_INVALID_ARGUMENT
        );
        assert!(last_error().contains("null"));
        assert!(ferrite_engine_create(null(), null(), null()).is_null());
        let missing = CString::new("/no/such/catalogues").unwrap();
        assert!(ferrite_engine_create(missing.as_ptr(), null(), null()).is_null());
        assert!(!last_error().is_empty());
        assert!(ferrite_engine_pick_json(null_mut(), 0., 0., 1.).is_null());
        assert_eq!(ferrite_engine_dataset_count(null_mut()), -2);
        ferrite_engine_destroy(null_mut());
        ferrite_string_free(null_mut());
    }
}

#[test]
fn opens_bytes_renders_and_picks() {
    let root = repository();
    let cell = root.join(
        "TestData/SHOM-S101/Data/diffusionS101_May_June/S-101_June2026/6940/101FR00369400_002.000",
    );
    let Ok(bytes) = std::fs::read(&cell) else {
        eprintln!("TestData not present; skipped");
        return;
    };
    let catalogues = CString::new(root.join("Catalogues").to_str().unwrap()).unwrap();
    unsafe {
        let engine = ferrite_engine_create(catalogues.as_ptr(), null(), null());
        assert!(!engine.is_null(), "{}", last_error());
        let mut rgba = vec![0u8; 64 * 48 * 4];
        assert_eq!(
            ferrite_engine_render_rgba(engine, rgba.as_mut_ptr(), rgba.len()),
            FERRITE_NO_SCENE
        );
        let name = CString::new("101FR00369400.000").unwrap();
        let names = [name.as_ptr()];
        let data = [bytes.as_ptr()];
        let lengths = [bytes.len()];
        assert_eq!(
            ferrite_engine_open_bytes(engine, names.as_ptr(), data.as_ptr(), lengths.as_ptr(), 1),
            FERRITE_OK,
            "{}",
            last_error()
        );
        assert_eq!(ferrite_engine_dataset_count(engine), 1);
        assert_eq!(
            ferrite_engine_prepare(engine, 64, 48, 1.0, null()),
            FERRITE_OK,
            "{}",
            last_error()
        );
        assert_eq!(
            ferrite_engine_render_rgba(engine, rgba.as_mut_ptr(), rgba.len() - 1),
            FERRITE_BUFFER_TOO_SMALL
        );
        let code = ferrite_engine_render_rgba(engine, rgba.as_mut_ptr(), rgba.len());
        if code == FERRITE_ERROR && last_error().contains("GPU") {
            eprintln!("no GPU adapter: {}", last_error());
        } else {
            assert_eq!(code, FERRITE_OK, "{}", last_error());
            let first = &rgba[..4];
            assert!(rgba.chunks(4).any(|p| p != first), "image is not uniform");
        }
        let report = ferrite_engine_report_json(engine);
        assert!(!report.is_null());
        let json: serde_json::Value =
            serde_json::from_str(CStr::from_ptr(report).to_str().unwrap()).unwrap();
        ferrite_string_free(report);
        assert_eq!(json["datasets"][0]["product"], "S-101");
        let hits = ferrite_engine_pick_json(engine, 32., 24., 30.);
        assert!(!hits.is_null(), "{}", last_error());
        let hits: serde_json::Value =
            serde_json::from_str(CStr::from_ptr(hits).to_str().unwrap()).unwrap();
        assert!(hits.is_array());
        let bad = CString::new("Nope").unwrap();
        assert_eq!(
            ferrite_engine_set_palette(engine, bad.as_ptr()),
            FERRITE_ERROR
        );
        assert!(last_error().contains("Nope"));
        ferrite_engine_destroy(engine);
    }
}
