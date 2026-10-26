//! `include/ferrite.h` is hand-written: every function it declares must be
//! exported by the library with C linkage, every exported function must be
//! declared, and the status codes must agree.
use ferrite::*;
use std::collections::BTreeSet;

fn header() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/include/ferrite.h")).unwrap()
}

/// Every `extern "C"` entry point of the crate, by name (the function
/// pointers make this fail to compile when one is renamed or removed).
fn exported() -> Vec<(&'static str, *const ())> {
    vec![
        ("ferrite_version", ferrite_version as *const ()),
        ("ferrite_last_error", ferrite_last_error as *const ()),
        ("ferrite_string_free", ferrite_string_free as *const ()),
        ("ferrite_engine_create", ferrite_engine_create as *const ()),
        (
            "ferrite_engine_destroy",
            ferrite_engine_destroy as *const (),
        ),
        (
            "ferrite_engine_open_folder",
            ferrite_engine_open_folder as *const (),
        ),
        (
            "ferrite_engine_open_files",
            ferrite_engine_open_files as *const (),
        ),
        (
            "ferrite_engine_open_bytes",
            ferrite_engine_open_bytes as *const (),
        ),
        (
            "ferrite_engine_dataset_count",
            ferrite_engine_dataset_count as *const (),
        ),
        (
            "ferrite_engine_set_palette",
            ferrite_engine_set_palette as *const (),
        ),
        (
            "ferrite_engine_set_precomputed",
            ferrite_engine_set_precomputed as *const (),
        ),
        (
            "ferrite_engine_prepare",
            ferrite_engine_prepare as *const (),
        ),
        (
            "ferrite_engine_render_rgba",
            ferrite_engine_render_rgba as *const (),
        ),
        (
            "ferrite_engine_pick_json",
            ferrite_engine_pick_json as *const (),
        ),
        (
            "ferrite_engine_report_json",
            ferrite_engine_report_json as *const (),
        ),
    ]
}

#[test]
fn header_declares_exactly_the_exported_functions() {
    let header = header();
    // Identifiers starting with `ferrite_` directly followed by `(`.
    let mut declared = BTreeSet::new();
    for (start, _) in header.match_indices("ferrite_") {
        let rest = &header[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if rest[end..].starts_with('(') {
            declared.insert(rest[..end].to_string());
        }
    }
    let exported: BTreeSet<String> = exported()
        .iter()
        .map(|(name, pointer)| {
            assert!(!pointer.is_null());
            name.to_string()
        })
        .collect();
    assert_eq!(declared, exported);
    // Also present in the crate source as `#[no_mangle] extern "C"`.
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).unwrap();
    for name in &exported {
        assert!(
            source.contains(&format!("extern \"C\" fn {name}(")),
            "{name} is not extern \"C\""
        );
    }
    assert_eq!(source.matches("#[no_mangle]").count(), exported.len());
}

#[test]
fn status_codes_agree() {
    let header = header();
    for (name, value) in [
        ("FERRITE_OK", FERRITE_OK),
        ("FERRITE_ERROR", FERRITE_ERROR),
        ("FERRITE_INVALID_ARGUMENT", FERRITE_INVALID_ARGUMENT),
        ("FERRITE_NO_SCENE", FERRITE_NO_SCENE),
        ("FERRITE_BUFFER_TOO_SMALL", FERRITE_BUFFER_TOO_SMALL),
        ("FERRITE_PANIC", FERRITE_PANIC),
    ] {
        let expected = if value < 0 {
            format!("#define {name} ({value})")
        } else {
            format!("#define {name} {value}")
        };
        assert!(header.contains(&expected), "{expected}");
    }
    assert_eq!(
        std::mem::size_of::<FerriteCamera>(),
        8 + 4 * 8,
        "FerriteCamera layout (int32 + padding, four doubles)"
    );
}
