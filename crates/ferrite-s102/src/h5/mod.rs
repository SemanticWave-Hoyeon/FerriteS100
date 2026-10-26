//! Backend-neutral, read-only HDF5 access for the S-102 adapter.
//!
//! Two readers sit behind one API: the native HDF5 C library (`hdf5-metno`,
//! feature `hdf5`, the default for local paths) and a pure-Rust reader
//! ([`pure`], always built) used for in-memory sources and wherever the C
//! library is absent (WebAssembly). Type descriptors and read conversions
//! follow `hdf5-metno` so that S-102 decoding is identical with either
//! reader; `tests/reader_parity.rs` checks this file by file.
#[cfg(feature = "hdf5")]
mod native;
pub mod pure;

use anyhow::{bail, ensure, Context, Result};
use std::{ops::Range, path::Path, sync::Arc};

/// Which reader decodes a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The HDF5 C library (feature `hdf5`).
    Native,
    /// The pure-Rust reader.
    Pure,
}
/// Selects the reader for local paths: `pure` or `native` (default native when built).
pub const READER_ENV: &str = "FERRITE_S102_HDF5_READER";
impl Backend {
    /// The reader for local paths: native when built, otherwise pure.
    /// `FERRITE_S102_HDF5_READER=pure` forces the pure reader.
    pub fn default_for_paths() -> Self {
        if !cfg!(feature = "hdf5") || std::env::var_os(READER_ENV).is_some_and(|v| v == "pure") {
            Backend::Pure
        } else {
            Backend::Native
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntSize {
    U1,
    U2,
    U4,
    U8,
}
impl IntSize {
    fn from_bytes(n: usize) -> Result<Self> {
        Ok(match n {
            1 => Self::U1,
            2 => Self::U2,
            4 => Self::U4,
            8 => Self::U8,
            _ => bail!("Invalid size of integer datatype"),
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FloatSize {
    U4,
    U8,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumMember {
    pub name: String,
    pub value: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumType {
    pub size: IntSize,
    pub signed: bool,
    pub members: Vec<EnumMember>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundField {
    pub name: String,
    pub ty: TypeDescriptor,
    pub offset: usize,
    pub index: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundType {
    pub fields: Vec<CompoundField>,
    pub size: usize,
}
/// Mirror of `hdf5::types::TypeDescriptor` for the classes S-102 can meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeDescriptor {
    Integer(IntSize),
    Unsigned(IntSize),
    Float(FloatSize),
    Boolean,
    Enum(EnumType),
    Compound(CompoundType),
    FixedArray(Box<TypeDescriptor>, usize),
    FixedAscii(usize),
    FixedUnicode(usize),
    VarLenArray(Box<TypeDescriptor>),
    VarLenAscii,
    VarLenUnicode,
    /// Object/region references (never decoded by this adapter).
    Reference,
}

/// Owned text of a variable-length ASCII string (C-string semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarLenAscii(Vec<u8>);
/// Owned text of a variable-length UTF-8 string (C-string semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarLenUnicode(Vec<u8>);
/// Fixed-width ASCII string read into `N` null-padded bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedAscii<const N: usize>(Vec<u8>);
/// Fixed-width UTF-8 string read into `N` null-padded bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedUnicode<const N: usize>(Vec<u8>);
macro_rules! text_type {
    ($($t:ty),*) => {$(
        impl $t {
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }
            /// Text when valid UTF-8; empty otherwise (callers validate bytes first).
            pub fn as_str(&self) -> &str {
                std::str::from_utf8(&self.0).unwrap_or("")
            }
        }
    )*};
}
text_type!(VarLenAscii, VarLenUnicode);
impl<const N: usize> FixedAscii<N> {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
impl<const N: usize> FixedUnicode<N> {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

fn cstr(mut bytes: Vec<u8>) -> Vec<u8> {
    if let Some(n) = bytes.iter().position(|b| *b == 0) {
        bytes.truncate(n);
    }
    bytes
}

/// HDF5 fixed-string conversion into an `n`-byte null-padded buffer
/// (H5T__conv_s_s), followed by `FixedAscii::as_bytes` trailing-NUL trim.
fn fixed_text(raw: &[u8], pad: u8, n: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = match pad {
        0 | 1 => raw
            .iter()
            .take(n)
            .take_while(|b| **b != 0)
            .copied()
            .collect(),
        2 => {
            let mut end = raw.len();
            while end > 0 && raw[end - 1] == b' ' {
                end -= 1;
            }
            raw[..end.min(n)].to_vec()
        }
        _ => bail!("source string padding method not supported"),
    };
    while out.last() == Some(&0) {
        out.pop();
    }
    Ok(out)
}

/// A value the backends can read (one element of a dataset or attribute).
pub trait H5Read: Sized + Send {
    #[cfg(feature = "hdf5")]
    #[doc(hidden)]
    type Native: hdf5::H5Type;
    #[cfg(feature = "hdf5")]
    #[doc(hidden)]
    fn from_native(value: Self::Native) -> Self;
    #[doc(hidden)]
    fn decode(dt: &pure::Dt, raw: &[u8], ctx: &pure::Ctx) -> Result<Self>;
}

enum Num {
    I(i128),
    F(f64),
}
/// Up to 8 bytes in little-endian order (no allocation).
fn ordered(raw: &[u8], size: usize, big_endian: bool) -> Result<[u8; 8]> {
    ensure!(size <= 8 && raw.len() >= size, "Truncated HDF5 element");
    let mut b = [0u8; 8];
    b[..size].copy_from_slice(&raw[..size]);
    if big_endian {
        b[..size].reverse();
    }
    Ok(b)
}
fn number(dt: &pure::Dt, raw: &[u8]) -> Result<Num> {
    use pure::Dt;
    Ok(match dt {
        Dt::Int {
            size,
            signed,
            big_endian,
        } => {
            let u = u64::from_le_bytes(ordered(raw, *size, *big_endian)?);
            if *signed && *size < 8 && u >> (size * 8 - 1) & 1 == 1 {
                Num::I(u as i128 - (1i128 << (size * 8)))
            } else if *signed && *size == 8 {
                Num::I(i128::from(u as i64))
            } else {
                Num::I(i128::from(u))
            }
        }
        Dt::Enum { base, .. } => number(base, raw)?,
        Dt::Float { size, big_endian } => {
            let b = ordered(raw, *size, *big_endian)?;
            Num::F(if *size == 4 {
                f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            } else {
                f64::from_le_bytes(b)
            })
        }
        other => bail!("Cannot convert HDF5 {other:?} to a number"),
    })
}
macro_rules! int_read {
    ($($t:ty),*) => {$(
        impl H5Read for $t {
            #[cfg(feature = "hdf5")]
            type Native = $t;
            #[cfg(feature = "hdf5")]
            fn from_native(value: $t) -> $t { value }
            fn decode(dt: &pure::Dt, raw: &[u8], _: &pure::Ctx) -> Result<$t> {
                Ok(match number(dt, raw)? {
                    Num::I(v) => v.clamp(<$t>::MIN as i128, <$t>::MAX as i128) as $t,
                    Num::F(f) => f as $t,
                })
            }
        }
    )*};
}
int_read!(u8, u16, u32, u64, i8, i16, i32, i64);
fn exact_float(dt: &pure::Dt, raw: &[u8], size: usize) -> Option<[u8; 8]> {
    match dt {
        pure::Dt::Float {
            size: s,
            big_endian,
        } if *s == size => ordered(raw, size, *big_endian).ok(),
        _ => None,
    }
}
impl H5Read for f32 {
    #[cfg(feature = "hdf5")]
    type Native = f32;
    #[cfg(feature = "hdf5")]
    fn from_native(value: f32) -> f32 {
        value
    }
    fn decode(dt: &pure::Dt, raw: &[u8], _: &pure::Ctx) -> Result<f32> {
        if let Some(b) = exact_float(dt, raw, 4) {
            return Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        }
        Ok(match number(dt, raw)? {
            Num::I(v) => v as f32,
            Num::F(f) => f as f32,
        })
    }
}
impl H5Read for f64 {
    #[cfg(feature = "hdf5")]
    type Native = f64;
    #[cfg(feature = "hdf5")]
    fn from_native(value: f64) -> f64 {
        value
    }
    fn decode(dt: &pure::Dt, raw: &[u8], _: &pure::Ctx) -> Result<f64> {
        if let Some(b) = exact_float(dt, raw, 8) {
            return Ok(f64::from_le_bytes(b));
        }
        Ok(match number(dt, raw)? {
            Num::I(v) => v as f64,
            Num::F(f) => f,
        })
    }
}
fn vlen_text(dt: &pure::Dt, raw: &[u8], ctx: &pure::Ctx) -> Result<Vec<u8>> {
    match dt {
        pure::Dt::VlenStr { .. } => Ok(cstr(ctx.vlen(raw, 1)?)),
        other => bail!("Cannot convert HDF5 {other:?} to a variable-length string"),
    }
}
impl H5Read for VarLenAscii {
    #[cfg(feature = "hdf5")]
    type Native = hdf5::types::VarLenAscii;
    #[cfg(feature = "hdf5")]
    fn from_native(value: Self::Native) -> Self {
        Self(value.as_bytes().to_vec())
    }
    fn decode(dt: &pure::Dt, raw: &[u8], ctx: &pure::Ctx) -> Result<Self> {
        vlen_text(dt, raw, ctx).map(Self)
    }
}
impl H5Read for VarLenUnicode {
    #[cfg(feature = "hdf5")]
    type Native = hdf5::types::VarLenUnicode;
    #[cfg(feature = "hdf5")]
    fn from_native(value: Self::Native) -> Self {
        Self(value.as_bytes().to_vec())
    }
    fn decode(dt: &pure::Dt, raw: &[u8], ctx: &pure::Ctx) -> Result<Self> {
        vlen_text(dt, raw, ctx).map(Self)
    }
}
fn fixed(dt: &pure::Dt, raw: &[u8], n: usize) -> Result<Vec<u8>> {
    match dt {
        pure::Dt::Str { size, pad, .. } => fixed_text(&raw[..*size], *pad, n),
        other => bail!("Cannot convert HDF5 {other:?} to a fixed string"),
    }
}
impl<const N: usize> H5Read for FixedAscii<N> {
    #[cfg(feature = "hdf5")]
    type Native = hdf5::types::FixedAscii<N>;
    #[cfg(feature = "hdf5")]
    fn from_native(value: Self::Native) -> Self {
        Self(value.as_bytes().to_vec())
    }
    fn decode(dt: &pure::Dt, raw: &[u8], _: &pure::Ctx) -> Result<Self> {
        fixed(dt, raw, N).map(Self)
    }
}
impl<const N: usize> H5Read for FixedUnicode<N> {
    #[cfg(feature = "hdf5")]
    type Native = hdf5::types::FixedUnicode<N>;
    #[cfg(feature = "hdf5")]
    fn from_native(value: Self::Native) -> Self {
        Self(value.as_bytes().to_vec())
    }
    fn decode(dt: &pure::Dt, raw: &[u8], _: &pure::Ctx) -> Result<Self> {
        fixed(dt, raw, N).map(Self)
    }
}

/// The datatype as the C library reports it after `H5Dget_type`/`H5Aget_type`
/// (H5T__set_loc to memory): compound members sorted by offset, variable-length
/// members resized to their memory form (pointer / `hvl_t`) and later members
/// shifted by the accumulated change. Decoding always uses the file layout.
fn memory_form(dt: &pure::Dt, so: usize) -> (pure::Dt, usize) {
    use pure::Dt;
    match dt {
        Dt::VlenStr { .. } => (dt.clone(), std::mem::size_of::<usize>()),
        Dt::VlenSeq { .. } => (dt.clone(), 2 * std::mem::size_of::<usize>()),
        Dt::Array { dims, base } => {
            let (b, size) = memory_form(base, so);
            let n: usize = dims.iter().map(|d| *d as usize).product();
            (
                Dt::Array {
                    dims: dims.clone(),
                    base: Box::new(b),
                },
                n * size,
            )
        }
        Dt::Compound { size, members } => {
            let mut sorted = members.clone();
            sorted.sort_by_key(|m| m.offset);
            let mut accum: isize = 0;
            for m in &mut sorted {
                let old = m.dt.size(so) as isize;
                let (mdt, new) = memory_form(&m.dt, so);
                m.offset = (m.offset as isize + accum) as usize;
                m.dt = mdt;
                accum += new as isize - old;
            }
            let size = (*size as isize + accum) as usize;
            (
                Dt::Compound {
                    size,
                    members: sorted,
                },
                size,
            )
        }
        other => (other.clone(), other.size(so)),
    }
}

/// Descriptor of a pure datatype, as `hdf5::Datatype::to_descriptor` reports
/// it for a dataset or attribute type (see [`memory_form`]).
pub fn descriptor_in(dt: &pure::Dt, so: usize) -> Result<TypeDescriptor> {
    descriptor(&memory_form(dt, so).0)
}

/// Descriptor of a pure datatype in the given layout.
pub fn descriptor(dt: &pure::Dt) -> Result<TypeDescriptor> {
    use pure::Dt;
    Ok(match dt {
        Dt::Int { size, signed, .. } => {
            let s = IntSize::from_bytes(*size)?;
            if *signed {
                TypeDescriptor::Integer(s)
            } else {
                TypeDescriptor::Unsigned(s)
            }
        }
        Dt::Float { size: 4, .. } => TypeDescriptor::Float(FloatSize::U4),
        Dt::Float { size: 8, .. } => TypeDescriptor::Float(FloatSize::U8),
        Dt::Float { .. } => bail!("Invalid size of float datatype"),
        Dt::Str { size, cset, .. } => match cset {
            0 => TypeDescriptor::FixedAscii(*size),
            1 => TypeDescriptor::FixedUnicode(*size),
            _ => bail!("Invalid encoding for string datatype"),
        },
        Dt::VlenStr { cset } => match cset {
            0 => TypeDescriptor::VarLenAscii,
            1 => TypeDescriptor::VarLenUnicode,
            _ => bail!("Invalid encoding for string datatype"),
        },
        Dt::VlenSeq { base } => TypeDescriptor::VarLenArray(Box::new(descriptor(base)?)),
        Dt::Array { dims, base } => {
            ensure!(
                dims.len() == 1,
                "Multi-dimensional array datatypes are not supported"
            );
            TypeDescriptor::FixedArray(Box::new(descriptor(base)?), dims[0] as usize)
        }
        Dt::Compound { size, members } => TypeDescriptor::Compound(CompoundType {
            fields: members
                .iter()
                .enumerate()
                .map(|(index, m)| {
                    Ok(CompoundField {
                        name: m.name.clone(),
                        ty: descriptor(&m.dt)?,
                        offset: m.offset,
                        index,
                    })
                })
                .collect::<Result<_>>()?,
            size: *size,
        }),
        Dt::Enum { base, members } => {
            let (size, signed, big) = match base.as_ref() {
                Dt::Int {
                    size,
                    signed,
                    big_endian,
                } => (*size, *signed, *big_endian),
                _ => bail!("Invalid base type for enum datatype"),
            };
            let members: Vec<EnumMember> = members
                .iter()
                .map(|(name, raw)| {
                    let mut b = raw.clone();
                    if big {
                        b.reverse();
                    }
                    EnumMember {
                        name: name.clone(),
                        value: pure::io::le(&b),
                    }
                })
                .collect();
            let size = IntSize::from_bytes(size)?;
            let boolean = size == IntSize::U1
                && members
                    == [
                        EnumMember {
                            name: "FALSE".into(),
                            value: 0,
                        },
                        EnumMember {
                            name: "TRUE".into(),
                            value: 1,
                        },
                    ];
            if boolean {
                TypeDescriptor::Boolean
            } else {
                TypeDescriptor::Enum(EnumType {
                    size,
                    signed,
                    members,
                })
            }
        }
        Dt::Unsupported { class: 7, .. } => bail!("Unsupported reference datatype"),
        Dt::Unsupported { .. } => bail!("Unsupported datatype class"),
    })
}

/// Datatype handle with the `hdf5` crate's `to_descriptor` call shape.
pub struct Datatype(Result<TypeDescriptor, String>);
impl Datatype {
    pub fn to_descriptor(&self) -> Result<TypeDescriptor> {
        self.0.clone().map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[derive(Clone)]
enum GroupInner {
    #[cfg(feature = "hdf5")]
    Native(hdf5::Group),
    Pure(pure::Group),
}
#[derive(Clone)]
pub struct Group(GroupInner);
pub struct File {
    root: Group,
    backend: Backend,
    #[cfg(feature = "hdf5")]
    _native: Option<hdf5::File>,
}
impl std::ops::Deref for File {
    type Target = Group;
    fn deref(&self) -> &Group {
        &self.root
    }
}
impl File {
    /// Open a local file with the default path reader.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, Backend::default_for_paths())
    }
    pub fn open_with(path: impl AsRef<Path>, backend: Backend) -> Result<Self> {
        let path = path.as_ref();
        match backend {
            #[cfg(feature = "hdf5")]
            Backend::Native => {
                let file = hdf5::File::open(path)?;
                Ok(Self {
                    root: Group(GroupInner::Native((*file).clone())),
                    backend,
                    _native: Some(file),
                })
            }
            #[cfg(not(feature = "hdf5"))]
            Backend::Native => bail!("The native HDF5 reader is not built (feature hdf5)"),
            Backend::Pure => {
                #[cfg(any(unix, windows))]
                let image: Arc<dyn pure::io::ReadAt> = Arc::new(pure::io::FileImage::open(path)?);
                #[cfg(not(any(unix, windows)))]
                let image: Arc<dyn pure::io::ReadAt> = Arc::new(pure::io::MemoryImage(
                    std::fs::read(path)
                        .with_context(|| format!("Cannot read {}", path.display()))?
                        .into(),
                ));
                Self::open_image(image)
            }
        }
    }
    /// Open an in-memory file image with the pure reader.
    pub fn open_bytes(bytes: impl Into<Arc<[u8]>>) -> Result<Self> {
        Self::open_image(Arc::new(pure::io::MemoryImage(bytes.into())))
    }
    fn open_image(image: Arc<dyn pure::io::ReadAt>) -> Result<Self> {
        let file = pure::File::open(image)?;
        Ok(Self {
            root: Group(GroupInner::Pure(file.root().clone())),
            backend: Backend::Pure,
            #[cfg(feature = "hdf5")]
            _native: None,
        })
    }
    pub fn backend(&self) -> Backend {
        self.backend
    }
}

#[cfg(feature = "hdf5")]
impl From<&hdf5::Group> for Group {
    /// Wrap a native group (tests and native callers).
    fn from(g: &hdf5::Group) -> Self {
        Group(GroupInner::Native(g.clone()))
    }
}
impl Group {
    pub fn attr(&self, name: &str) -> Result<Attribute> {
        Ok(match &self.0 {
            #[cfg(feature = "hdf5")]
            GroupInner::Native(g) => Attribute(AttrInner::Native(g.attr(name)?)),
            GroupInner::Pure(g) => Attribute(AttrInner::Pure(g.attr(name)?)),
        })
    }
    pub fn attr_names(&self) -> Result<Vec<String>> {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            GroupInner::Native(g) => Ok(g.attr_names()?),
            GroupInner::Pure(g) => g.attr_names(),
        }
    }
    pub fn member_names(&self) -> Result<Vec<String>> {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            GroupInner::Native(g) => Ok(g.member_names()?),
            GroupInner::Pure(g) => Ok(g.member_names()),
        }
    }
    pub fn link_exists(&self, name: &str) -> bool {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            GroupInner::Native(g) => g.link_exists(name),
            GroupInner::Pure(g) => g.link_exists(name),
        }
    }
    pub fn group(&self, name: &str) -> Result<Group> {
        Ok(match &self.0 {
            #[cfg(feature = "hdf5")]
            GroupInner::Native(g) => Group(GroupInner::Native(g.group(name)?)),
            GroupInner::Pure(g) => Group(GroupInner::Pure(g.group(name)?)),
        })
    }
    pub fn dataset(&self, name: &str) -> Result<Dataset> {
        Ok(match &self.0 {
            #[cfg(feature = "hdf5")]
            GroupInner::Native(g) => Dataset(DsInner::Native(g.dataset(name)?)),
            GroupInner::Pure(g) => Dataset(DsInner::Pure(g.dataset(name)?)),
        })
    }
}

#[derive(Clone)]
enum AttrInner {
    #[cfg(feature = "hdf5")]
    Native(hdf5::Attribute),
    Pure(pure::Attribute),
}
#[derive(Clone)]
pub struct Attribute(AttrInner);

#[derive(Clone)]
enum DsInner {
    #[cfg(feature = "hdf5")]
    Native(hdf5::Dataset),
    Pure(pure::Dataset),
}
#[derive(Clone)]
pub struct Dataset(DsInner);
impl std::fmt::Debug for Dataset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            DsInner::Native(d) => write!(f, "{d:?}"),
            DsInner::Pure(d) => write!(f, "{d:?}"),
        }
    }
}

fn pure_shape(shape: &Option<Vec<u64>>) -> Vec<usize> {
    shape
        .as_ref()
        .map(|s| s.iter().map(|v| *v as usize).collect())
        .unwrap_or_default()
}
fn decode_all<T: H5Read>(e: pure::Elements, ctx: &pure::Ctx) -> Result<Vec<T>> {
    if e.esize == 0 {
        return Ok(Vec::new());
    }
    e.data
        .chunks_exact(e.esize)
        .map(|raw| T::decode(&e.dt, raw, ctx))
        .collect()
}
fn member<'a>(dt: &'a pure::Dt, name: &str) -> Result<&'a pure::Member> {
    match dt {
        pure::Dt::Compound { members, .. } => members
            .iter()
            .find(|m| m.name == name)
            .with_context(|| format!("HDF5 compound has no member '{name}'")),
        _ => bail!("HDF5 dataset is not compound"),
    }
}

macro_rules! container_common {
    ($t:ident, $inner:ident) => {
        impl $t {
            pub fn shape(&self) -> Vec<usize> {
                match &self.0 {
                    #[cfg(feature = "hdf5")]
                    $inner::Native(a) => a.shape(),
                    $inner::Pure(a) => pure_shape(&a.shape),
                }
            }
            pub fn ndim(&self) -> usize {
                self.shape().len()
            }
            pub fn size(&self) -> usize {
                self.shape().iter().product()
            }
            pub fn is_scalar(&self) -> bool {
                match &self.0 {
                    #[cfg(feature = "hdf5")]
                    $inner::Native(a) => a.is_scalar(),
                    $inner::Pure(a) => a.shape.as_ref().is_some_and(|s| s.is_empty()),
                }
            }
            pub fn dtype(&self) -> Result<Datatype> {
                Ok(Datatype(match &self.0 {
                    #[cfg(feature = "hdf5")]
                    $inner::Native(a) => native::descriptor(&a.dtype()?),
                    $inner::Pure(a) => {
                        descriptor_in(&a.dt, a.ctx().so).map_err(|e| format!("{e:#}"))
                    }
                }))
            }
        }
    };
}
container_common!(Attribute, AttrInner);
container_common!(Dataset, DsInner);

impl Attribute {
    /// Every element, converted as the `hdf5` crate's `read_raw` does.
    pub fn read_raw<T: H5Read>(&self) -> Result<Vec<T>> {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            AttrInner::Native(a) => Ok(a
                .read_raw::<T::Native>()?
                .into_iter()
                .map(T::from_native)
                .collect()),
            AttrInner::Pure(a) => decode_all(a.elements(), a.ctx()),
        }
    }
}

impl Dataset {
    pub fn backend(&self) -> Backend {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            DsInner::Native(_) => Backend::Native,
            DsInner::Pure(_) => Backend::Pure,
        }
    }
    pub fn read_raw<T: H5Read>(&self) -> Result<Vec<T>> {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            DsInner::Native(d) => Ok(d
                .read_raw::<T::Native>()?
                .into_iter()
                .map(T::from_native)
                .collect()),
            DsInner::Pure(d) => decode_all(d.elements(None)?, d.ctx()),
        }
    }
    fn window(&self, rows: &Range<usize>, cols: &Range<usize>) -> Result<([u64; 2], [u64; 2])> {
        let shape = self.shape();
        ensure!(
            shape.len() == 2
                && rows.start <= rows.end
                && cols.start <= cols.end
                && rows.end <= shape[0]
                && cols.end <= shape[1],
            "HDF5 window outside 2-D dataset"
        );
        Ok((
            [rows.start as u64, cols.start as u64],
            [rows.len() as u64, cols.len() as u64],
        ))
    }
    /// A row-major 2-D window of a scalar-typed dataset.
    pub fn read_slice_2d<T: H5Read>(
        &self,
        rows: Range<usize>,
        cols: Range<usize>,
    ) -> Result<Vec<T>> {
        let (start, count) = self.window(&rows, &cols)?;
        match &self.0 {
            #[cfg(feature = "hdf5")]
            DsInner::Native(d) => Ok(native::read_window::<T::Native>(d, None, start, count)?
                .into_iter()
                .map(T::from_native)
                .collect()),
            DsInner::Pure(d) => decode_all(d.elements(Some((&start, &count)))?, d.ctx()),
        }
    }
    /// One named member of every element of a compound dataset.
    pub fn read_field<T: H5Read>(&self, name: &str) -> Result<Vec<T>> {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            DsInner::Native(d) => Ok(native::read_member::<T::Native>(d, name)?
                .into_iter()
                .map(T::from_native)
                .collect()),
            DsInner::Pure(d) => {
                let m = member(&d.dt, name)?.clone();
                let e = d.elements(None)?;
                e.data
                    .chunks_exact(e.esize)
                    .map(|raw| T::decode(&m.dt, &raw[m.offset..], d.ctx()))
                    .collect()
            }
        }
    }
    /// Named float32 members of a compound 2-D window, interleaved per element.
    pub fn read_f32_fields_2d(
        &self,
        names: &[&str],
        rows: Range<usize>,
        cols: Range<usize>,
    ) -> Result<Vec<f32>> {
        let (start, count) = self.window(&rows, &cols)?;
        ensure!(!names.is_empty() && names.len() <= 8, "Invalid member list");
        match &self.0 {
            #[cfg(feature = "hdf5")]
            DsInner::Native(d) => native::read_f32_members(d, names, start, count),
            DsInner::Pure(d) => {
                let members = names
                    .iter()
                    .map(|n| member(&d.dt, n).cloned())
                    .collect::<Result<Vec<_>>>()?;
                let e = d.elements(Some((&start, &count)))?;
                let mut out = Vec::with_capacity(e.data.len() / e.esize.max(1) * names.len());
                for raw in e.data.chunks_exact(e.esize) {
                    for m in &members {
                        out.push(f32::decode(&m.dt, &raw[m.offset..], d.ctx())?);
                    }
                }
                Ok(out)
            }
        }
    }
    /// Bytes the C library would allocate for this dataset's variable-length
    /// strings (each including its NUL), i.e. `H5Dvlen_get_buf_size`.
    pub fn vlen_buf_size(&self) -> Result<u64> {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            DsInner::Native(d) => native::vlen_buf_size(d, None),
            DsInner::Pure(d) => pure_vlen_size(d, None),
        }
    }
    /// As [`Self::vlen_buf_size`] for one string member of a compound dataset;
    /// `unicode` selects the memory string type (as `VarLenUnicode`).
    pub fn field_vlen_buf_size(&self, name: &str, unicode: bool) -> Result<u64> {
        match &self.0 {
            #[cfg(feature = "hdf5")]
            DsInner::Native(d) => native::vlen_buf_size(d, Some((name, unicode))),
            DsInner::Pure(d) => {
                let _ = unicode;
                pure_vlen_size(d, Some(name))
            }
        }
    }
}

fn pure_vlen_size(d: &pure::Dataset, field: Option<&str>) -> Result<u64> {
    let (dt, offset) = match field {
        Some(name) => {
            let m = member(&d.dt, name)?;
            (m.dt.clone(), m.offset)
        }
        None => (d.dt.clone(), 0),
    };
    ensure!(
        matches!(dt, pure::Dt::VlenStr { .. }),
        "Not a variable-length string"
    );
    let e = d.elements(None)?;
    let mut total = 0u64;
    for raw in e.data.chunks_exact(e.esize) {
        total = total
            .checked_add(d.ctx().vlen_len(&raw[offset..])? + 1)
            .context("Variable-length size overflow")?;
    }
    Ok(total)
}
