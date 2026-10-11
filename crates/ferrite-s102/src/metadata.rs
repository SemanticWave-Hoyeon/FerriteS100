//! Encoded axis and publication metadata; never a request to transpose raw values.
use anyhow::{ensure, Context, Result};
use hdf5::{
    types::{FixedAscii, FixedUnicode, TypeDescriptor, VarLenAscii, VarLenUnicode},
    Group,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisNamesOrder {
    ArrayMajorFirst,
    ScanFastFirst,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxisMetadata {
    pub names: [String; 2],
    pub order: AxisNamesOrder,
    pub variable_length: bool,
}
impl AxisMetadata {
    /// S1005.2.0 10c-9.11 major index precedes minor. S1024.4 still fixes
    /// canonical row-major east-then-north association. Preserve a contradictory
    /// declared axis order as an explicit diagnostic; do not secretly transpose.
    pub fn matches_generic_array_major_order(&self) -> bool {
        self.order == AxisNamesOrder::ArrayMajorFirst
    }
    pub(crate) fn read(container: &Group, crs: u32) -> Result<Self> {
        let d = container
            .dataset("axisNames")
            .context("Missing S102 axisNames dataset")?;
        ensure!(
            d.shape() == [2],
            "S102 axisNames must be one-dimensional with two entries"
        );
        let ty = d.dtype()?;
        let desc = ty.to_descriptor()?;
        let variable = matches!(
            desc,
            TypeDescriptor::VarLenAscii | TypeDescriptor::VarLenUnicode
        );
        let ascii = matches!(
            desc,
            TypeDescriptor::VarLenAscii | TypeDescriptor::FixedAscii(_)
        );
        if variable {
            let space = d.space()?;
            let mut bytes = 0;
            // IDs remain owned/live, and the same HDF5 recursive lock covers this FFI.
            let result = hdf5::sync::sync(|| unsafe {
                hdf5_sys::h5d::H5Dvlen_get_buf_size(d.id(), ty.id(), space.id(), &mut bytes)
            });
            ensure!(
                result >= 0,
                "Cannot forecast axisNames variable string size"
            );
            ensure!(
                bytes <= 130,
                "axisNames variable payload exceeds supported130 bytes"
            );
        }
        let raw: Vec<Vec<u8>> = match desc {
            TypeDescriptor::VarLenAscii => d
                .read_raw::<VarLenAscii>()?
                .into_iter()
                .map(|v| v.as_bytes().to_vec())
                .collect(),
            TypeDescriptor::VarLenUnicode => d
                .read_raw::<VarLenUnicode>()?
                .into_iter()
                .map(|v| v.as_bytes().to_vec())
                .collect(),
            TypeDescriptor::FixedAscii(n) => {
                ensure!(n <= 64, "axisNames fixed strings exceed64 bytes");
                d.read_raw::<FixedAscii<64>>()?
                    .into_iter()
                    .map(|v| v.as_bytes().to_vec())
                    .collect()
            }
            TypeDescriptor::FixedUnicode(n) => {
                ensure!(n <= 64, "axisNames fixed strings exceed64 bytes");
                d.read_raw::<FixedUnicode<64>>()?
                    .into_iter()
                    .map(|v| v.as_bytes().to_vec())
                    .collect()
            }
            _ => anyhow::bail!("axisNames requires HDF5 string entries"),
        };
        let mut names = Vec::with_capacity(2);
        for bytes in raw {
            ensure!(
                bytes.len() <= 64 && (!ascii || bytes.is_ascii()),
                "Invalid axisNames length/ASCII encoding"
            );
            names.push(
                std::str::from_utf8(&bytes)
                    .context("Invalid axisNames UTF8")?
                    .to_owned(),
            );
        }
        let names: [String; 2] = names
            .try_into()
            .map_err(|_| anyhow::anyhow!("Missing axisNames entry"))?;
        let fast = crate::canonical_axes(crs)?;
        let order = if names[0] == fast[1] && names[1] == fast[0] {
            AxisNamesOrder::ArrayMajorFirst
        } else if names[0] == fast[0] && names[1] == fast[1] {
            AxisNamesOrder::ScanFastFirst
        } else {
            anyhow::bail!("axisNames disagree with S102 horizontal CRS axes");
        };
        Ok(Self {
            names,
            order,
            variable_length: variable,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueMetadata {
    pub date: String,
    pub time: Option<String>,
}
impl IssueMetadata {
    pub(crate) fn read(root: &Group) -> Result<Self> {
        let date =
            crate::string_attr(root, "issueDate").context("Missing/invalid S102 issueDate")?;
        ensure!(
            date.len() == 8 && date.bytes().all(|v| v.is_ascii_digit()),
            "S102 issueDate requires complete basic YYYYMMDD"
        );
        ferrite_kernel::parse_viewing_date(&date).context("Invalid S102 issueDate calendar")?;
        let time = if root.attr_names()?.iter().any(|v| v == "issueTime") {
            let t = crate::string_attr(root, "issueTime")?;
            validate_basic_time(&t)?;
            Some(t)
        } else {
            None
        };
        Ok(Self { date, time })
    }
}
// Lexical Time validation, with no invented timezone/precision conversion.
// Leap-second60 and decimal fractions stay raw: this does not authenticate a
// historical UTC leap event or expand the kernel's temporal-selector arithmetic.
fn validate_basic_time(value: &str) -> Result<()> {
    ensure!(
        value.is_ascii() && (6..=64).contains(&value.len()),
        "S102 issueTime requires basic ASCII Time"
    );
    let body = if let Some(body) = value.strip_suffix('Z') {
        body
    } else if let Some(i) = value.bytes().position(|v| v == b'+' || v == b'-') {
        let zone = &value[i..];
        ensure!(
            zone.len() == 5 && zone[1..].bytes().all(|v| v.is_ascii_digit()),
            "Invalid issueTime basic UTC offset"
        );
        ensure!(
            zone[1..3].parse::<u8>()? < 24 && zone[3..5].parse::<u8>()? < 60,
            "Invalid issueTime UTC offset range"
        );
        &value[..i]
    } else {
        value
    };
    let (main, fraction) = if let Some(i) = body.bytes().position(|v| v == b'.' || v == b',') {
        (&body[..i], Some(&body[i + 1..]))
    } else {
        (body, None)
    };
    ensure!(
        main.len() == 6 && main.bytes().all(|v| v.is_ascii_digit()),
        "issueTime requires complete HHMMSS"
    );
    ensure!(
        fraction.is_none_or(|s| !s.is_empty() && s.bytes().all(|v| v.is_ascii_digit())),
        "Invalid issueTime decimal fraction"
    );
    let h = main[..2].parse::<u8>()?;
    let m = main[2..4].parse::<u8>()?;
    let seconds = main[4..].parse::<u8>()?;
    ensure!(
        h <= 24 && m < 60 && seconds <= 60,
        "Invalid issueTime clock range"
    );
    ensure!(
        h != 24 || m == 0 && seconds == 0 && fraction.is_none_or(|s| s.bytes().all(|v| v == b'0')),
        "issueTime24-hour boundary must be midnight"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct File(std::path::PathBuf, hdf5::File);
    impl Drop for File {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn file() -> File {
        let p = std::env::temp_dir().join(format!(
            "s102-axis-issue-{}-{}.h5",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let f = hdf5::File::create(&p).unwrap();
        File(p, f)
    }
    fn text(g: &Group, n: &str, s: &str) {
        g.new_attr::<VarLenAscii>()
            .create(n)
            .unwrap()
            .write_scalar(&VarLenAscii::from_ascii(s).unwrap())
            .unwrap();
    }
    fn axes(g: &Group, a: [&str; 2]) {
        g.new_dataset::<VarLenAscii>()
            .shape(2)
            .create("axisNames")
            .unwrap()
            .write_raw(&a.map(|s| VarLenAscii::from_ascii(s).unwrap()))
            .unwrap();
    }
    #[test]
    fn basic_publication_calendar_optional_clock_and_raw_zone_are_retained() {
        for d in ["00010101", "20000229", "20240229", "99991231"] {
            let f = file();
            text(&f.1, "issueDate", d);
            assert_eq!(
                IssueMetadata::read(&f.1).unwrap(),
                IssueMetadata {
                    date: d.into(),
                    time: None
                }
            );
        }
        for t in [
            "123000",
            "123000Z",
            "123000+0100",
            "123000-0500",
            "123000.125Z",
            "123000,125Z",
            "235960Z",
            "123000.123456789123Z",
            "240000Z",
        ] {
            let f = file();
            text(&f.1, "issueDate", "20240229");
            text(&f.1, "issueTime", t);
            let a = IssueMetadata::read(&f.1).unwrap();
            assert_eq!(a.date, "20240229");
            assert_eq!(a.time.as_deref(), Some(t));
        }
    }
    #[test]
    fn missing_non_scalar_incomplete_and_invalid_publication_metadata_rejected() {
        let f = file();
        assert!(IssueMetadata::read(&f.1).is_err());
        for d in [
            "20230229",
            "20240230",
            "19000229",
            "2024-02-29",
            "2024----",
            "20241301",
            "20240100",
        ] {
            let f = file();
            text(&f.1, "issueDate", d);
            assert!(IssueMetadata::read(&f.1).is_err(), "{d}");
        }
        for t in [
            "25",
            "250000Z",
            "236060Z",
            "123061Z",
            "240001Z",
            "123000.Z",
            "12:30:00Z",
            "123000+2500",
            "123000Zjunk",
        ] {
            let f = file();
            text(&f.1, "issueDate", "20240229");
            text(&f.1, "issueTime", t);
            assert!(IssueMetadata::read(&f.1).is_err(), "{t}");
        }
        // A single-element array (UKHO 2026) is admitted as the scalar value;
        // any other shape remains rejected.
        let f = file();
        f.1.new_attr::<VarLenAscii>()
            .shape(1)
            .create("issueDate")
            .unwrap()
            .write_raw(&[VarLenAscii::from_ascii("20240229").unwrap()])
            .unwrap();
        assert_eq!(IssueMetadata::read(&f.1).unwrap().date, "20240229");
        let f = file();
        f.1.new_attr::<VarLenAscii>()
            .shape(2)
            .create("issueDate")
            .unwrap()
            .write_raw(&[
                VarLenAscii::from_ascii("20240229").unwrap(),
                VarLenAscii::from_ascii("20240301").unwrap(),
            ])
            .unwrap();
        assert!(IssueMetadata::read(&f.1).is_err());
    }
    #[test]
    fn geographic_and_projected_axis_order_is_preserved_and_diagnosed() {
        for (crs, names) in [
            (4326, ["Longitude", "Latitude"]),
            (32631, ["Easting", "Northing"]),
            (32731, ["Easting", "Northing"]),
            (5041, ["Easting", "Northing"]),
            (5042, ["Easting", "Northing"]),
        ] {
            let f = file();
            axes(&f.1, names);
            let a = AxisMetadata::read(&f.1, crs).unwrap();
            assert_eq!(a.names, names);
            assert_eq!(a.order, AxisNamesOrder::ScanFastFirst);
            assert!(!a.matches_generic_array_major_order());
            let f = file();
            axes(&f.1, [names[1], names[0]]);
            assert!(AxisMetadata::read(&f.1, crs)
                .unwrap()
                .matches_generic_array_major_order());
        }
    }
    #[test]
    fn ascii_utf8_and_fixed_string_arrays_checked_without_label_aliases() {
        let f = file();
        f.1.new_dataset::<VarLenUnicode>()
            .shape(2)
            .create("axisNames")
            .unwrap()
            .write_raw(&["Latitude", "Longitude"].map(|v| v.parse::<VarLenUnicode>().unwrap()))
            .unwrap();
        assert!(AxisMetadata::read(&f.1, 4326).unwrap().variable_length);
        let f = file();
        f.1.new_dataset::<FixedAscii<12>>()
            .shape(2)
            .create("axisNames")
            .unwrap()
            .write_raw(&["Latitude", "Longitude"].map(|v| FixedAscii::<12>::from_ascii(v).unwrap()))
            .unwrap();
        assert!(!AxisMetadata::read(&f.1, 4326).unwrap().variable_length);
        for names in [
            ["latitude", "longitude"],
            ["X", "Y"],
            ["Latitude", "Latitude"],
            ["Longitude ", "Latitude"],
            ["Easting", "Northing"],
        ] {
            let f = file();
            axes(&f.1, names);
            assert!(AxisMetadata::read(&f.1, 4326).is_err());
        }
    }
    #[test]
    fn dataset_admission_checks_shape_type_and_variable_payload_before_decode() {
        let f = file();
        assert!(AxisMetadata::read(&f.1, 4326).is_err());
        let f = file();
        f.1.new_dataset::<u32>()
            .shape(2)
            .create("axisNames")
            .unwrap();
        assert!(AxisMetadata::read(&f.1, 4326).is_err());
        let f = file();
        f.1.new_dataset::<VarLenAscii>()
            .shape([1, 2])
            .create("axisNames")
            .unwrap();
        assert!(AxisMetadata::read(&f.1, 4326).is_err());
        let f = file();
        axes(&f.1, [&"a".repeat(1000), "Latitude"]);
        assert!(AxisMetadata::read(&f.1, 4326)
            .unwrap_err()
            .to_string()
            .contains("variable payload"));
    }
}

#[cfg(test)]
mod invalid_encoding_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct File(std::path::PathBuf, hdf5::File);
    impl Drop for File {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn file() -> File {
        let p = std::env::temp_dir().join(format!(
            "s102-raw-encoding-{}-{}.h5",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let f = hdf5::File::create(&p).unwrap();
        File(p, f)
    }
    #[test]
    fn oversized_attribute_payloads_are_admitted_before_rust_owned_copy() {
        let huge = vec![b'a'; 1024 * 1024];
        for name in ["issueDate", "issueTime", "timePoint"] {
            assert!(crate::checked_metadata_text(&huge, true, name)
                .unwrap_err()
                .to_string()
                .contains("string exceeds"));
        }
        for (name, limit) in [("issueDate", 8), ("issueTime", 64), ("timePoint", 4096)] {
            assert!(crate::checked_metadata_text(&vec![b'a'; limit], true, name).is_ok());
            assert!(crate::checked_metadata_text(&vec![b'a'; limit + 1], true, name).is_err());
        }
        let f = file();
        f.1.new_attr::<VarLenAscii>()
            .create("issueDate")
            .unwrap()
            .write_scalar(&VarLenAscii::from_ascii(&huge).unwrap())
            .unwrap();
        assert!(crate::string_attr(&f.1, "issueDate")
            .unwrap_err()
            .to_string()
            .contains("string exceeds"));
    }
    #[test]
    fn declared_utf8_metadata_is_checked_before_any_str_interpretation() {
        let f = file();
        let a = f.1.new_attr::<VarLenUnicode>().create("issueDate").unwrap();
        let ty = a.dtype().unwrap();
        let raw = [0xffu8, 0];
        let ptr = raw.as_ptr();
        // Author invalid SOURCE bytes via C API, never construct an invalid Rust str.
        let status = hdf5::sync::sync(|| unsafe {
            hdf5_sys::h5a::H5Awrite(a.id(), ty.id(), (&ptr as *const *const u8).cast())
        });
        assert!(status >= 0);
        assert!(crate::string_attr(&f.1, "issueDate")
            .unwrap_err()
            .to_string()
            .contains("UTF8"));
    }
    #[test]
    fn declared_utf8_axis_array_invalid_bytes_are_rejected() {
        let f = file();
        let d =
            f.1.new_dataset::<VarLenUnicode>()
                .shape(2)
                .create("axisNames")
                .unwrap();
        let ty = d.dtype().unwrap();
        let bad = [0xffu8, 0];
        let good = b"Longitude\0";
        let raw = [bad.as_ptr(), good.as_ptr()];
        let status = hdf5::sync::sync(|| unsafe {
            hdf5_sys::h5d::H5Dwrite(
                d.id(),
                ty.id(),
                hdf5_sys::h5s::H5S_ALL,
                hdf5_sys::h5s::H5S_ALL,
                hdf5_sys::h5p::H5P_DEFAULT,
                raw.as_ptr().cast(),
            )
        });
        assert!(status >= 0);
        assert!(AxisMetadata::read(&f.1, 4326)
            .unwrap_err()
            .to_string()
            .contains("UTF8"));
    }
}
