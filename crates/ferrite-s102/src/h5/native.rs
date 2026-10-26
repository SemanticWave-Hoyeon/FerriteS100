//! The HDF5 C library behind the backend-neutral API.
use super::{
    CompoundField, CompoundType, EnumMember, EnumType, FloatSize, IntSize, TypeDescriptor,
};
use anyhow::{ensure, Context, Result};
use hdf5::types::TypeDescriptor as TD;
use hdf5::H5Type;
use hdf5_sys::{
    h5::hsize_t,
    h5d::{H5Dget_space, H5Dread, H5Dvlen_get_buf_size},
    h5i::hid_t,
    h5p::H5P_DEFAULT,
    h5s::{H5S_seloper_t, H5Sclose, H5Screate_simple, H5Sselect_hyperslab, H5S_ALL},
};
use std::mem::MaybeUninit;

fn int(s: hdf5::types::IntSize) -> IntSize {
    match s {
        hdf5::types::IntSize::U1 => IntSize::U1,
        hdf5::types::IntSize::U2 => IntSize::U2,
        hdf5::types::IntSize::U4 => IntSize::U4,
        hdf5::types::IntSize::U8 => IntSize::U8,
    }
}
fn convert(t: &TD) -> std::result::Result<TypeDescriptor, String> {
    Ok(match t {
        TD::Integer(s) => TypeDescriptor::Integer(int(*s)),
        TD::Unsigned(s) => TypeDescriptor::Unsigned(int(*s)),
        TD::Float(hdf5::types::FloatSize::U4) => TypeDescriptor::Float(FloatSize::U4),
        TD::Float(hdf5::types::FloatSize::U8) => TypeDescriptor::Float(FloatSize::U8),
        TD::Boolean => TypeDescriptor::Boolean,
        TD::Enum(e) => TypeDescriptor::Enum(EnumType {
            size: int(e.size),
            signed: e.signed,
            members: e
                .members
                .iter()
                .map(|m| EnumMember {
                    name: m.name.clone(),
                    value: m.value,
                })
                .collect(),
        }),
        TD::Compound(c) => TypeDescriptor::Compound(CompoundType {
            fields: c
                .fields
                .iter()
                .map(|f| {
                    Ok(CompoundField {
                        name: f.name.clone(),
                        ty: convert(&f.ty)?,
                        offset: f.offset,
                        index: f.index,
                    })
                })
                .collect::<std::result::Result<_, String>>()?,
            size: c.size,
        }),
        TD::FixedArray(t, n) => TypeDescriptor::FixedArray(Box::new(convert(t)?), *n),
        TD::FixedAscii(n) => TypeDescriptor::FixedAscii(*n),
        TD::FixedUnicode(n) => TypeDescriptor::FixedUnicode(*n),
        TD::VarLenArray(t) => TypeDescriptor::VarLenArray(Box::new(convert(t)?)),
        TD::VarLenAscii => TypeDescriptor::VarLenAscii,
        TD::VarLenUnicode => TypeDescriptor::VarLenUnicode,
        TD::Reference(_) => TypeDescriptor::Reference,
        #[allow(unreachable_patterns)]
        other => return Err(format!("Unsupported datatype {other:?}")),
    })
}
pub fn descriptor(dt: &hdf5::Datatype) -> std::result::Result<TypeDescriptor, String> {
    convert(&dt.to_descriptor().map_err(|e| e.to_string())?)
}

/// Memory type: a one-member compound naming `field` at offset 0 (the HDF5
/// library converts the matching file member), or `T` itself.
fn memory_type<T: H5Type>(field: Option<&str>) -> Result<hdf5::Datatype> {
    let desc = match field {
        Some(name) => TD::Compound(hdf5::types::CompoundType {
            fields: vec![hdf5::types::CompoundField {
                name: name.into(),
                ty: T::type_descriptor(),
                offset: 0,
                index: 0,
            }],
            size: std::mem::size_of::<T>(),
        }),
        None => T::type_descriptor(),
    };
    Ok(hdf5::Datatype::from_descriptor(&desc)?)
}

/// Read `count` elements into `T` with the given memory type and selection.
fn read_into<T>(
    d: &hdf5::Dataset,
    mem: &hdf5::Datatype,
    n: usize,
    window: Option<([u64; 2], [u64; 2])>,
) -> Result<Vec<T>> {
    let mut buf: Vec<MaybeUninit<T>> = Vec::new();
    buf.try_reserve_exact(n)?;
    let status = hdf5::sync::sync(|| unsafe {
        let (mspace, fspace) = match window {
            None => (H5S_ALL, H5S_ALL),
            Some((start, count)) => {
                let fspace = H5Dget_space(d.id());
                if fspace < 0 {
                    return -1;
                }
                let start: [hsize_t; 2] = [start[0] as _, start[1] as _];
                let count: [hsize_t; 2] = [count[0] as _, count[1] as _];
                if H5Sselect_hyperslab(
                    fspace,
                    H5S_seloper_t::H5S_SELECT_SET,
                    start.as_ptr(),
                    std::ptr::null(),
                    count.as_ptr(),
                    std::ptr::null(),
                ) < 0
                {
                    H5Sclose(fspace);
                    return -1;
                }
                let mspace = H5Screate_simple(2, count.as_ptr(), std::ptr::null());
                if mspace < 0 {
                    H5Sclose(fspace);
                    return -1;
                }
                (mspace, fspace)
            }
        };
        let status = H5Dread(
            d.id(),
            mem.id(),
            mspace,
            fspace,
            H5P_DEFAULT,
            buf.as_mut_ptr().cast(),
        );
        if mspace != H5S_ALL {
            H5Sclose(mspace);
            H5Sclose(fspace);
        }
        status
    });
    ensure!(status >= 0, "HDF5 read failed");
    // SAFETY: H5Dread initialized all n selected elements.
    unsafe { buf.set_len(n) };
    Ok(buf
        .into_iter()
        .map(|v| unsafe { v.assume_init() })
        .collect())
}

pub fn read_window<T: H5Type>(
    d: &hdf5::Dataset,
    field: Option<&str>,
    start: [u64; 2],
    count: [u64; 2],
) -> Result<Vec<T>> {
    let n = usize::try_from(count[0] * count[1])?;
    read_into(d, &memory_type::<T>(field)?, n, Some((start, count)))
}

pub fn read_member<T: H5Type>(d: &hdf5::Dataset, name: &str) -> Result<Vec<T>> {
    read_into(d, &memory_type::<T>(Some(name))?, d.size(), None)
}

pub fn read_f32_members(
    d: &hdf5::Dataset,
    names: &[&str],
    start: [u64; 2],
    count: [u64; 2],
) -> Result<Vec<f32>> {
    let desc = TD::Compound(hdf5::types::CompoundType {
        fields: names
            .iter()
            .enumerate()
            .map(|(i, n)| hdf5::types::CompoundField {
                name: (*n).into(),
                ty: f32::type_descriptor(),
                offset: 4 * i,
                index: i,
            })
            .collect(),
        size: 4 * names.len(),
    });
    let mem = hdf5::Datatype::from_descriptor(&desc)?;
    let floats = usize::try_from(count[0] * count[1])?
        .checked_mul(names.len())
        .context("HDF5 window overflow")?;
    read_packed(d, &mem, floats, start, count)
}

fn read_packed(
    d: &hdf5::Dataset,
    mem: &hdf5::Datatype,
    floats: usize,
    start: [u64; 2],
    count: [u64; 2],
) -> Result<Vec<f32>> {
    // Read into a flat f32 buffer: the memory compound is `floats / elements`
    // consecutive f32 values per element, so the buffer layout is identical.
    let mut buf: Vec<MaybeUninit<f32>> = Vec::new();
    buf.try_reserve_exact(floats)?;
    let status = hdf5::sync::sync(|| unsafe {
        let fspace = H5Dget_space(d.id());
        if fspace < 0 {
            return -1;
        }
        let s: [hsize_t; 2] = [start[0] as _, start[1] as _];
        let c: [hsize_t; 2] = [count[0] as _, count[1] as _];
        if H5Sselect_hyperslab(
            fspace,
            H5S_seloper_t::H5S_SELECT_SET,
            s.as_ptr(),
            std::ptr::null(),
            c.as_ptr(),
            std::ptr::null(),
        ) < 0
        {
            H5Sclose(fspace);
            return -1;
        }
        let mspace = H5Screate_simple(2, c.as_ptr(), std::ptr::null());
        if mspace < 0 {
            H5Sclose(fspace);
            return -1;
        }
        let status = H5Dread(
            d.id(),
            mem.id(),
            mspace,
            fspace,
            H5P_DEFAULT,
            buf.as_mut_ptr().cast(),
        );
        H5Sclose(mspace);
        H5Sclose(fspace);
        status
    });
    ensure!(status >= 0, "HDF5 read failed");
    // SAFETY: H5Dread wrote every member of every selected element.
    unsafe { buf.set_len(floats) };
    Ok(buf
        .into_iter()
        .map(|v| unsafe { v.assume_init() })
        .collect())
}

pub fn vlen_buf_size(d: &hdf5::Dataset, field: Option<(&str, bool)>) -> Result<u64> {
    let ty = match field {
        None => d.dtype()?,
        Some((name, false)) => memory_type::<hdf5::types::VarLenAscii>(Some(name))?,
        Some((name, true)) => memory_type::<hdf5::types::VarLenUnicode>(Some(name))?,
    };
    let space = d.space()?;
    let mut size: hsize_t = 0;
    let id: hid_t = d.id();
    let status =
        hdf5::sync::sync(|| unsafe { H5Dvlen_get_buf_size(id, ty.id(), space.id(), &mut size) });
    ensure!(status >= 0, "Cannot forecast HDF5 variable-length payload");
    Ok(size as u64)
}
