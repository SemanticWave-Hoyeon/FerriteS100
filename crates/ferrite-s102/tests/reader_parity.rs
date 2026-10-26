//! The pure-Rust HDF5 reader against the HDF5 C library: every S-102 file
//! in TestData and synthetic fixtures covering the format variants must give
//! identical structure, attributes, values and S-102 decoding.
#![cfg(feature = "hdf5")]
use ferrite_kernel::CoverageSource;
use ferrite_s102::h5::{
    self, Backend, FixedAscii, FixedUnicode, TypeDescriptor, VarLenAscii, VarLenUnicode,
};
use ferrite_s102::BathymetryCoverage;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn s102_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![workspace().join("TestData")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("h5"))
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("102"))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Every element of an attribute or dataset rendered by its descriptor.
fn values_attr(a: &h5::Attribute, desc: &TypeDescriptor) -> String {
    render(desc, |kind| match kind {
        Kind::I => fmt(a.read_raw::<i64>()),
        Kind::U => fmt(a.read_raw::<u64>()),
        Kind::F => fmt(a
            .read_raw::<f64>()
            .map(|v| v.into_iter().map(f64::to_bits).collect())),
        Kind::FA => fmt(a.read_raw::<FixedAscii<1024>>().map(bytes)),
        Kind::FU => fmt(a.read_raw::<FixedUnicode<1024>>().map(bytes)),
        Kind::VA => fmt(a.read_raw::<VarLenAscii>().map(bytes)),
        Kind::VU => fmt(a.read_raw::<VarLenUnicode>().map(bytes)),
        Kind::Field(..) => "compound attribute".into(),
    })
}
fn values_dataset(d: &h5::Dataset, desc: &TypeDescriptor) -> String {
    if let TypeDescriptor::Compound(c) = desc {
        let mut out = String::new();
        for f in &c.fields {
            let name = f.name.as_str();
            let v = render(&f.ty, |kind| match kind {
                Kind::I => fmt(d.read_field::<i64>(name)),
                Kind::U => fmt(d.read_field::<u64>(name)),
                Kind::F => fmt(d
                    .read_field::<f64>(name)
                    .map(|v| v.into_iter().map(f64::to_bits).collect())),
                Kind::FA => fmt(d.read_field::<FixedAscii<1024>>(name).map(bytes)),
                Kind::FU => fmt(d.read_field::<FixedUnicode<1024>>(name).map(bytes)),
                Kind::VA => fmt(d.read_field::<VarLenAscii>(name).map(bytes)),
                Kind::VU => fmt(d.read_field::<VarLenUnicode>(name).map(bytes)),
                Kind::Field(..) => "nested".into(),
            });
            let _ = writeln!(out, "  .{name} = {v}");
            if matches!(
                f.ty,
                TypeDescriptor::VarLenAscii | TypeDescriptor::VarLenUnicode
            ) {
                let unicode = f.ty == TypeDescriptor::VarLenUnicode;
                let _ = writeln!(
                    out,
                    "  .{name} vlen = {:?}",
                    d.field_vlen_buf_size(name, unicode).ok()
                );
            }
        }
        return out;
    }
    let mut out = render(desc, |kind| match kind {
        Kind::I => fmt(d.read_raw::<i64>()),
        Kind::U => fmt(d.read_raw::<u64>()),
        Kind::F => fmt(d
            .read_raw::<f64>()
            .map(|v| v.into_iter().map(f64::to_bits).collect())),
        Kind::FA => fmt(d.read_raw::<FixedAscii<1024>>().map(bytes)),
        Kind::FU => fmt(d.read_raw::<FixedUnicode<1024>>().map(bytes)),
        Kind::VA => fmt(d.read_raw::<VarLenAscii>().map(bytes)),
        Kind::VU => fmt(d.read_raw::<VarLenUnicode>().map(bytes)),
        Kind::Field(..) => unreachable!(),
    });
    if matches!(
        desc,
        TypeDescriptor::VarLenAscii | TypeDescriptor::VarLenUnicode
    ) {
        let _ = write!(out, " vlen={:?}", d.vlen_buf_size().ok());
    }
    let shape = d.shape();
    if shape.len() == 2
        && matches!(
            desc,
            TypeDescriptor::Unsigned(_) | TypeDescriptor::Integer(_)
        )
    {
        // A window through the middle exercises hyperslab selection.
        let rows = shape[0] / 3..(shape[0] * 2 / 3).max(shape[0] / 3);
        let cols = shape[1] / 4..shape[1];
        let _ = write!(out, " window={}", fmt(d.read_slice_2d::<u64>(rows, cols)));
    }
    out
}
enum Kind {
    I,
    U,
    F,
    FA,
    FU,
    VA,
    VU,
    #[allow(dead_code)]
    Field(()),
}
fn render(desc: &TypeDescriptor, read: impl Fn(Kind) -> String) -> String {
    match desc {
        TypeDescriptor::Integer(_) => read(Kind::I),
        TypeDescriptor::Unsigned(_) | TypeDescriptor::Boolean => read(Kind::U),
        TypeDescriptor::Enum(e) if e.signed => read(Kind::I),
        TypeDescriptor::Enum(_) => read(Kind::U),
        TypeDescriptor::Float(_) => read(Kind::F),
        TypeDescriptor::FixedAscii(_) => read(Kind::FA),
        TypeDescriptor::FixedUnicode(_) => read(Kind::FU),
        TypeDescriptor::VarLenAscii => read(Kind::VA),
        TypeDescriptor::VarLenUnicode => read(Kind::VU),
        other => format!("not compared: {other:?}"),
    }
}
trait AsBytes {
    fn bytes_of(&self) -> Vec<u8>;
}
macro_rules! as_bytes {
    ($($t:ty),*) => {$(impl AsBytes for $t { fn bytes_of(&self) -> Vec<u8> { self.as_bytes().to_vec() } })*};
}
as_bytes!(
    VarLenAscii,
    VarLenUnicode,
    FixedAscii<1024>,
    FixedUnicode<1024>
);
fn bytes<T: AsBytes>(v: Vec<T>) -> Vec<Vec<u8>> {
    v.iter().map(AsBytes::bytes_of).collect()
}
/// Digest of long value lists; errors compare by presence only (the two
/// readers word their messages differently).
fn fmt<T: std::fmt::Debug>(v: anyhow::Result<Vec<T>>) -> String {
    match v {
        Ok(v) if v.len() > 64 => {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            format!("{v:?}").hash(&mut h);
            format!("[{} values, hash {:016x}]", v.len(), h.finish())
        }
        Ok(v) => format!("{v:?}"),
        Err(_) => "error".into(),
    }
}

/// Structure, attributes and values of the whole file.
fn dump(file: &h5::File) -> String {
    let mut out = String::new();
    dump_group(file, "/", &mut out, 0);
    out
}
fn dump_attrs(attrs: Vec<(String, h5::Attribute)>, out: &mut String) {
    for (name, a) in attrs {
        let desc = a.dtype().and_then(|t| t.to_descriptor());
        let _ = write!(
            out,
            "  @{name} shape={:?} scalar={} ",
            a.shape(),
            a.is_scalar()
        );
        match desc {
            Ok(desc) => {
                let _ = writeln!(out, "{desc:?} = {}", values_attr(&a, &desc));
            }
            Err(_) => {
                let _ = writeln!(out, "unsupported type");
            }
        }
    }
}
fn dump_group(g: &h5::Group, path: &str, out: &mut String, depth: usize) {
    assert!(depth < 16);
    let _ = writeln!(out, "group {path}");
    let mut names = g.attr_names().unwrap();
    names.sort();
    dump_attrs(
        names
            .into_iter()
            .map(|n| {
                let a = g.attr(&n).unwrap();
                (n, a)
            })
            .collect(),
        out,
    );
    let mut members = g.member_names().unwrap();
    // Native iteration order follows the HDF5 index; callers sort.
    members.sort();
    for m in members {
        let child = format!(
            "{}{m}",
            if path == "/" {
                "/".into()
            } else {
                format!("{path}/")
            }
        );
        if let Ok(sub) = g.group(&m) {
            dump_group(&sub, &child, out, depth + 1);
        } else {
            match g.dataset(&m) {
                Ok(d) => {
                    let desc = d.dtype().and_then(|t| t.to_descriptor());
                    let _ = write!(out, "dataset {child} shape={:?} ", d.shape());
                    match desc {
                        Ok(desc) => {
                            let _ = writeln!(out, "{desc:?}\n{}", values_dataset(&d, &desc));
                        }
                        Err(_) => {
                            let _ = writeln!(out, "unsupported type");
                        }
                    }
                }
                Err(_) => {
                    let _ = writeln!(out, "other {child}");
                }
            }
        }
    }
}

/// S-102 decoding through the adapter: metadata, every sample, quality ids.
fn decode(path: &Path, backend: Backend) -> String {
    let mut out = String::new();
    match BathymetryCoverage::open_with_backend(path, backend) {
        Ok(coverages) => {
            for c in coverages {
                let g = *c.geometry();
                let _ = writeln!(
                    out,
                    "{} {:?} {:?} {:?} {:?} {:?} {:?} {} {} {} {} {} {} {} {}",
                    c.instance_name,
                    g,
                    c.issue,
                    c.axes,
                    c.domain,
                    c.root_enclosure,
                    c.uncertainty_encoding,
                    c.time_point,
                    c.vertical_crs,
                    c.vertical_datum,
                    c.declared_min_depth,
                    c.declared_max_depth,
                    c.declared_min_uncertainty,
                    c.declared_max_uncertainty,
                    c.horizontal_position_uncertainty
                );
                let _ = writeln!(out, "{:?}", c.feature_metadata);
                let tile = c
                    .read_window(ferrite_kernel::GridWindow {
                        column: 0,
                        row: 0,
                        width: g.width,
                        height: g.height,
                    })
                    .map(|t| {
                        t.samples
                            .iter()
                            .map(|s| (s.value.map(f32::to_bits), s.uncertainty.map(f32::to_bits)))
                            .collect::<Vec<_>>()
                    });
                let _ = writeln!(out, "samples {}", fmt(tile));
                if let Some(q) = &c.quality {
                    let mut records: Vec<_> = q.records().map(|r| format!("{r:?}")).collect();
                    records.sort();
                    let _ = writeln!(
                        out,
                        "quality records {}",
                        fmt(Ok::<_, anyhow::Error>(records))
                    );
                    let qg = *q.geometry();
                    let ids = q.read_window_ids(ferrite_kernel::GridWindow {
                        column: 0,
                        row: 0,
                        width: qg.width,
                        height: qg.height,
                    });
                    let _ = writeln!(out, "quality ids {}", fmt(ids));
                }
            }
        }
        Err(e) => {
            // The readers word low-level failures differently; S-102
            // contract failures (raised above HDF5) are compared verbatim.
            let text = format!("{e:#}");
            let adapter = !text.contains("HDF5") && !text.contains("H5");
            let _ = writeln!(
                out,
                "coverage error: {}",
                if adapter { text.as_str() } else { "(reader)" }
            );
        }
    }
    if let Ok(t) = ferrite_s102::legacy_trial::LegacyTrialGrid::open_file(
        h5::File::open_with(path, backend).unwrap(),
    ) {
        let g = t.geometry();
        let _ = writeln!(
            out,
            "trial {:?} {:?} {:?} {:?} {:?} {}",
            g,
            t.diagnostics,
            t.feature_definitions,
            t.declared_depth_extrema,
            t.vertical_datum,
            fmt(t
                .read_window(0, 0, g.width.min(200), g.height.min(200))
                .map(|s| s
                    .iter()
                    .map(|v| (v.depth.to_bits(), v.uncertainty.to_bits()))
                    .collect::<Vec<_>>()))
        );
    }
    out
}

fn compare(path: &Path) {
    let native = h5::File::open_with(path, Backend::Native).unwrap();
    let pure = h5::File::open_with(path, Backend::Pure)
        .unwrap_or_else(|e| panic!("pure reader refused {}: {e:#}", path.display()));
    let a = dump(&native);
    let b = dump(&pure);
    if a != b {
        let diff = a
            .lines()
            .zip(b.lines())
            .find(|(x, y)| x != y)
            .map(|(x, y)| format!("native: {x}\npure:   {y}"))
            .unwrap_or_else(|| "length differs".into());
        panic!("structure differs for {}:\n{diff}", path.display());
    }
    // In-memory images decode exactly like files.
    let bytes = h5::File::open_bytes(std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        dump(&bytes),
        b,
        "memory image differs for {}",
        path.display()
    );
    let da = decode(path, Backend::Native);
    let db = decode(path, Backend::Pure);
    assert_eq!(da, db, "S-102 decoding differs for {}", path.display());
}

#[test]
fn test_data_files_decode_identically() {
    let files = s102_files();
    if files.is_empty() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let mut seen = std::collections::HashSet::new();
    let mut compared = 0;
    for f in &files {
        let digest = {
            use sha2::Digest;
            sha2::Sha256::digest(std::fs::read(f).unwrap())
        };
        if !seen.insert(digest) {
            continue;
        }
        compare(f);
        compared += 1;
    }
    eprintln!(
        "compared {compared} distinct S-102 files ({} paths)",
        files.len()
    );
    assert!(compared > 0);
}

mod fixtures {
    use super::*;
    use hdf5::types::{VarLenAscii as NA, VarLenUnicode as NU};

    struct Tmp(tempfile::TempDir);
    impl Tmp {
        fn path(&self, n: &str) -> PathBuf {
            self.0.path().join(n)
        }
    }
    fn tmp() -> Tmp {
        Tmp(tempfile::tempdir().unwrap())
    }

    #[derive(hdf5::H5Type, Clone, Copy)]
    #[repr(C)]
    struct Depth {
        depth: f32,
        uncertainty: f32,
    }
    #[derive(hdf5::H5Type, Clone, Copy)]
    #[repr(u8)]
    enum Code {
        A = 1,
        B = 3,
    }
    #[derive(hdf5::H5Type, Clone)]
    #[repr(C)]
    struct Row {
        id: u32,
        name: NA,
        label: NU,
        code: Code,
        value: f64,
    }

    fn populate(f: &hdf5::File, chunked: bool, filters: bool) {
        for i in 0..20 {
            f.new_attr::<f32>()
                .create(format!("attr{i:02}").as_str())
                .unwrap()
                .write_scalar(&(i as f32 * 0.25))
                .unwrap();
        }
        f.new_attr::<i16>()
            .create("negative")
            .unwrap()
            .write_scalar(&-7i16)
            .unwrap();
        f.new_attr::<u64>()
            .shape(3)
            .create("vector")
            .unwrap()
            .write_raw(&[1u64, 2, u64::MAX])
            .unwrap();
        f.new_attr::<Code>()
            .create("code")
            .unwrap()
            .write_scalar(&Code::B)
            .unwrap();
        f.new_attr::<bool>()
            .create("flag")
            .unwrap()
            .write_scalar(&true)
            .unwrap();
        let text: NA = NA::from_ascii("INT.IHO.S-102.3.0.0").unwrap();
        f.new_attr::<NA>()
            .create("productSpecification")
            .unwrap()
            .write_scalar(&text)
            .unwrap();
        let utf: NU = "Déjà vu".parse().unwrap();
        f.new_attr::<NU>()
            .shape(1)
            .create("utf")
            .unwrap()
            .write_raw(&[utf])
            .unwrap();
        let fixed = hdf5::types::FixedAscii::<12>::from_ascii(b"20240131").unwrap();
        f.new_attr::<hdf5::types::FixedAscii<12>>()
            .create("fixed")
            .unwrap()
            .write_scalar(&fixed)
            .unwrap();
        let g = f.create_group("Container").unwrap();
        for i in 0..30 {
            g.create_group(format!("Child.{i:03}").as_str()).unwrap();
        }
        let (h, w) = (37usize, 53usize);
        let data: Vec<Depth> = (0..h * w)
            .map(|i| Depth {
                depth: i as f32 * 0.5 - 3.,
                uncertainty: if i % 7 == 0 { 1e6 } else { i as f32 * 0.01 },
            })
            .collect();
        let mut b = g.new_dataset::<Depth>().shape((h, w));
        if chunked {
            b = b.chunk((8, 16));
            if filters {
                b = b.shuffle().deflate(6).fletcher32();
            }
        }
        b.create("values").unwrap().write_raw(&data).unwrap();
        let ids: Vec<u32> = (0..h * w).map(|i| (i % 5) as u32).collect();
        let mut b = g.new_dataset::<u32>().shape((h, w));
        if chunked {
            b = b.chunk((10, 10));
            if filters {
                b = b.deflate(9);
            }
        }
        b.create("ids").unwrap().write_raw(&ids).unwrap();
        // Partially written chunked dataset: unwritten chunks are fill.
        if chunked {
            let d = g
                .new_dataset::<f32>()
                .shape((64, 64))
                .chunk((16, 16))
                .fill_value(-1f32)
                .create("sparse")
                .unwrap();
            d.write_slice(
                &ndarray::Array2::<f32>::from_elem((16, 16), 5.),
                (0..16, 16..32),
            )
            .unwrap();
        }
        let rows: Vec<Row> = (0..40)
            .map(|i| Row {
                id: i,
                name: NA::from_ascii(&"x".repeat(i as usize)).unwrap(),
                label: format!("ü{i}").parse().unwrap(),
                code: if i % 2 == 0 { Code::A } else { Code::B },
                value: f64::from(i) / 3.,
            })
            .collect();
        f.new_dataset::<Row>()
            .shape(40)
            .create("table")
            .unwrap()
            .write_raw(&rows)
            .unwrap();
        let names: Vec<NA> = ["Longitude", "Latitude"]
            .iter()
            .map(|s| NA::from_ascii(s).unwrap())
            .collect();
        f.new_dataset::<NA>()
            .shape(2)
            .create("axisNames")
            .unwrap()
            .write_raw(&names)
            .unwrap();
        f.new_dataset::<i8>()
            .create("scalar")
            .unwrap()
            .write_scalar(&-3i8)
            .unwrap();
    }

    fn check(file: &Path) {
        compare(file);
    }

    #[test]
    fn default_library_format() {
        let t = tmp();
        for (n, chunked, filters) in [
            ("c.h5", false, false),
            ("k.h5", true, false),
            ("z.h5", true, true),
        ] {
            let p = t.path(n);
            populate(&hdf5::File::create(&p).unwrap(), chunked, filters);
            check(&p);
        }
    }

    #[test]
    fn earliest_format_symbol_tables_and_v1_btrees() {
        let t = tmp();
        for (n, chunked, filters) in [("c.h5", false, false), ("z.h5", true, true)] {
            let p = t.path(n);
            let f = hdf5::File::with_options()
                .with_fapl(|p| p.libver_earliest())
                .create(&p)
                .unwrap();
            populate(&f, chunked, filters);
            drop(f);
            check(&p);
        }
    }

    #[test]
    fn latest_format_fixed_array_and_v2_btree_indexes() {
        let t = tmp();
        for (n, chunked, filters) in [
            ("c.h5", false, false),
            ("k.h5", true, false),
            ("z.h5", true, true),
        ] {
            let p = t.path(n);
            let f = hdf5::File::with_options()
                .with_fapl(|p| p.libver_latest())
                .create(&p)
                .unwrap();
            populate(&f, chunked, filters);
            // Two unlimited dimensions: v2 B-tree chunk index.
            let d = f
                .new_dataset::<f32>()
                .shape((20.., 30..))
                .chunk((7, 9))
                .deflate(3)
                .create("growable")
                .unwrap();
            let v: Vec<f32> = (0..600).map(|i| i as f32).collect();
            d.write_raw(&v).unwrap();
            drop(d);
            // A single chunk covering the whole dataset.
            f.new_dataset::<u16>()
                .shape((6, 5))
                .chunk((6, 5))
                .deflate(1)
                .create("single")
                .unwrap()
                .write_raw(&(0..30u16).collect::<Vec<_>>())
                .unwrap();
            drop(f);
            check(&p);
        }
    }

    #[test]
    fn unsupported_features_fail_closed() {
        let t = tmp();
        let p = t.path("ext.h5");
        let f = hdf5::File::with_options()
            .with_fapl(|p| p.libver_latest())
            .create(&p)
            .unwrap();
        // One unlimited dimension: extensible-array chunk index.
        f.new_dataset::<f32>()
            .shape((10.., 4))
            .chunk((5, 4))
            .create("ext")
            .unwrap()
            .write_raw(&[0f32; 40])
            .unwrap();
        drop(f);
        let pure = h5::File::open_with(&p, Backend::Pure).unwrap();
        let err = pure.dataset("ext").unwrap_err();
        assert!(format!("{err:#}").contains("extensible"), "{err:#}");
        assert!(h5::File::open_bytes(b"not an hdf5 file".to_vec()).is_err());
        // Truncated images are refused, never read past the end.
        let bytes = std::fs::read(&p).unwrap();
        for cut in [16, 100, bytes.len() / 2] {
            let r = h5::File::open_bytes(bytes[..cut].to_vec())
                .and_then(|f| f.dataset("ext").and_then(|d| d.read_raw::<f32>()));
            assert!(r.is_err());
        }
    }
}
