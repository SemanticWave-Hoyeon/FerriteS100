//! S-100 Part 10c metadata attributes are scalars. UKHO's April 2026 S-102 3.0
//! exchange set encodes every attribute as a one-element rank-1 dataspace
//! instead (as its S-102 2.1 trial data did, see `legacy_trial`). Accept
//! exactly these two shapes holding exactly one element; every datatype
//! contract of the callers is unchanged.
use anyhow::{ensure, Result};
use hdf5::{Attribute, H5Type};

/// Rank 0, or rank 1 with a single element.
pub fn admitted(a: &Attribute) -> bool {
    a.is_scalar() || a.shape() == [1]
}

/// The single value of an admitted attribute, with the same HDF5 datatype
/// conversion as `read_scalar`.
pub fn read<T: H5Type>(a: &Attribute) -> Result<T> {
    ensure!(
        admitted(a),
        "S-102 attribute must be scalar or a single element, found shape {:?}",
        a.shape()
    );
    let mut values = a.read_raw::<T>()?;
    ensure!(values.len() == 1, "S-102 attribute element count mismatch");
    Ok(values.pop().expect("one element checked"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdf5::types::VarLenUnicode;
    #[test]
    fn scalar_and_singleton_are_equal_and_other_shapes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = hdf5::File::create(dir.path().join("a.h5")).unwrap();
        file.new_attr::<f32>()
            .create("scalar")
            .unwrap()
            .write_scalar(&2.5f32)
            .unwrap();
        file.new_attr::<f32>()
            .shape([1])
            .create("singleton")
            .unwrap()
            .write(&[2.5f32])
            .unwrap();
        file.new_attr::<f32>()
            .shape([2])
            .create("pair")
            .unwrap()
            .write(&[1f32, 2.])
            .unwrap();
        let text: VarLenUnicode = "INT.IHO.S-102.3.0.0".parse().unwrap();
        file.new_attr::<VarLenUnicode>()
            .shape([1])
            .create("text")
            .unwrap()
            .write(std::slice::from_ref(&text))
            .unwrap();
        let scalar = file.attr("scalar").unwrap();
        let singleton = file.attr("singleton").unwrap();
        assert!(admitted(&scalar) && admitted(&singleton));
        assert_eq!(
            read::<f32>(&scalar).unwrap(),
            read::<f32>(&singleton).unwrap()
        );
        assert!(!admitted(&file.attr("pair").unwrap()));
        assert!(read::<f32>(&file.attr("pair").unwrap()).is_err());
        assert_eq!(
            read::<VarLenUnicode>(&file.attr("text").unwrap())
                .unwrap()
                .as_str(),
            text.as_str()
        );
    }
}
