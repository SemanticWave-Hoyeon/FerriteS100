//! Decode numeric metadata without HDF5's silent float/integer coercion or narrowing.
//! Integer widths remain receiver-compatible; this is not a full encoded-width
//! schema certificate. Enumerations retain their explicit HDF enum representation.
use anyhow::{ensure, Context, Result};
use hdf5::{
    types::{FloatSize, TypeDescriptor},
    Attribute, Group,
};
fn attribute(g: &Group, name: &str) -> Result<Attribute> {
    let a = g
        .attr(name)
        .with_context(|| format!("Missing S-102 {name}"))?;
    ensure!(a.is_scalar(), "S-102 {name} must be scalar");
    Ok(a)
}
fn nonnegative(g: &Group, name: &str) -> Result<u64> {
    let a = attribute(g, name)?;
    match a.dtype()?.to_descriptor()? {
        TypeDescriptor::Unsigned(_) => Ok(a.read_scalar::<u64>()?),
        TypeDescriptor::Integer(_) => {
            let v = a.read_scalar::<i64>()?;
            ensure!(v >= 0, "S-102 {name} integer must be nonnegative");
            Ok(v as u64)
        }
        TypeDescriptor::Enum(e) if !e.signed => Ok(a.read_scalar::<u64>()?),
        TypeDescriptor::Enum(_) => {
            let v = a.read_scalar::<i64>()?;
            ensure!(v >= 0, "S-102 {name} enumeration must be nonnegative");
            Ok(v as u64)
        }
        _ => anyhow::bail!(
            "S-102 {name} must be an integer or explicit enumeration; numeric coercion refused"
        ),
    }
}
pub(crate) fn u8(g: &Group, name: &str) -> Result<u8> {
    let v = nonnegative(g, name)?;
    u8::try_from(v).with_context(|| format!("S-102 {name} exceeds unsigned8 range"))
}
pub(crate) fn u32(g: &Group, name: &str) -> Result<u32> {
    let v = nonnegative(g, name)?;
    u32::try_from(v).with_context(|| format!("S-102 {name} exceeds unsigned32 range"))
}
// Explicitly unsigned fields retain the product specification's sign contract.
fn require_unsigned(g: &Group, name: &str) -> Result<()> {
    let a = attribute(g, name)?;
    ensure!(
        matches!(a.dtype()?.to_descriptor()?, TypeDescriptor::Unsigned(_)),
        "S-102 {name} must be an unsigned integer"
    );
    Ok(())
}
pub(crate) fn unsigned_u8(g: &Group, name: &str) -> Result<u8> {
    require_unsigned(g, name)?;
    u8(g, name)
}
pub(crate) fn unsigned_u32(g: &Group, name: &str) -> Result<u32> {
    require_unsigned(g, name)?;
    u32(g, name)
}
pub(crate) fn f32(g: &Group, name: &str) -> Result<f32> {
    let a = attribute(g, name)?;
    ensure!(
        a.dtype()?.to_descriptor()? == TypeDescriptor::Float(FloatSize::U4),
        "S-102 {name} must be scalar float32"
    );
    Ok(a.read_scalar::<f32>()?)
}
pub(crate) fn f64(g: &Group, name: &str) -> Result<f64> {
    let a = attribute(g, name)?;
    ensure!(
        a.dtype()?.to_descriptor()? == TypeDescriptor::Float(FloatSize::U8),
        "S-102 {name} must be scalar float64"
    );
    Ok(a.read_scalar::<f64>()?)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn attr<T: hdf5::H5Type>(g: &Group, n: &str, v: T) {
        g.new_attr::<T>()
            .create(n)
            .unwrap()
            .write_scalar(&v)
            .unwrap();
    }
    #[test]
    fn narrowing_and_float_integer_coercion_are_rejected() {
        let p = std::env::temp_dir().join(format!("ferrite-scalar-int-{}.h5", std::process::id()));
        {
            let f = hdf5::File::create(&p).unwrap();
            attr(&f, "float_enum", 2.5f64);
            attr(&f, "signed_count", 1i32);
            attr(&f, "wide_count", 256u16);
            attr(&f, "wide_grid", u64::from(u32::MAX) + 1);
            attr(&f, "u8max", 255u64);
            attr(&f, "u32max", u64::from(u32::MAX));
            assert!(u8(&f, "float_enum").is_err());
            assert!(unsigned_u32(&f, "signed_count").is_err());
            assert_eq!(u32(&f, "signed_count").unwrap(), 1);
            assert!(u8(&f, "wide_count").is_err());
            assert!(u32(&f, "wide_grid").is_err());
            assert_eq!(u8(&f, "u8max").unwrap(), 255);
            assert_eq!(u32(&f, "u32max").unwrap(), u32::MAX);
            f.new_attr::<u8>()
                .shape(1)
                .create("vector")
                .unwrap()
                .write_raw(&[2])
                .unwrap();
            assert!(u8(&f, "vector").is_err());
        }
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn floating_metadata_preserves_width_and_bits() {
        let p =
            std::env::temp_dir().join(format!("ferrite-scalar-float-{}.h5", std::process::id()));
        {
            let f = hdf5::File::create(&p).unwrap();
            attr(&f, "spacing", f64::from_bits(1));
            attr(&f, "range", -0.0f32);
            attr(&f, "integer", 1u32);
            assert_eq!(f64(&f, "spacing").unwrap().to_bits(), 1);
            assert_eq!(f32(&f, "range").unwrap().to_bits(), (-0.0f32).to_bits());
            assert!(f32(&f, "spacing").is_err());
            assert!(f64(&f, "range").is_err());
            assert!(f64(&f, "integer").is_err());
        }
        std::fs::remove_file(p).unwrap();
    }
}

#[cfg(test)]
mod enum_tests {
    use super::*;
    #[derive(hdf5::H5Type, Clone, Copy)]
    #[repr(u8)]
    enum Unsigned {
        One = 1,
        Max = 255,
    }
    #[derive(hdf5::H5Type, Clone, Copy)]
    #[repr(i16)]
    enum Signed {
        Negative = -1,
        One = 1,
        Large = 256,
    }
    #[derive(hdf5::H5Type, Clone, Copy)]
    #[repr(u64)]
    enum Wide {
        One = 1,
        Overflow = 4294967296,
    }
    fn attr<T: hdf5::H5Type>(g: &Group, n: &str, v: T) {
        g.new_attr::<T>()
            .create(n)
            .unwrap()
            .write_scalar(&v)
            .unwrap();
    }
    #[test]
    fn actual_hdf_enumeration_conversion_is_lossless_or_rejected() {
        let p = std::env::temp_dir().join(format!("ferrite-scalar-enum-{}.h5", std::process::id()));
        {
            let f = hdf5::File::create(&p).unwrap();
            attr(&f, "unsigned_one", Unsigned::One);
            attr(&f, "unsigned_max", Unsigned::Max);
            attr(&f, "signed_one", Signed::One);
            attr(&f, "signed_negative", Signed::Negative);
            attr(&f, "signed_large", Signed::Large);
            attr(&f, "wide_one", Wide::One);
            attr(&f, "wide_overflow", Wide::Overflow);
            assert_eq!(u8(&f, "unsigned_one").unwrap(), 1);
            assert_eq!(u8(&f, "unsigned_max").unwrap(), 255);
            assert_eq!(u8(&f, "signed_one").unwrap(), 1);
            assert_eq!(u32(&f, "signed_large").unwrap(), 256);
            assert!(u8(&f, "signed_large").is_err());
            assert!(u8(&f, "signed_negative").is_err());
            assert_eq!(u32(&f, "wide_one").unwrap(), 1);
            assert!(u32(&f, "wide_overflow").is_err());
        }
        std::fs::remove_file(p).unwrap();
    }
}
