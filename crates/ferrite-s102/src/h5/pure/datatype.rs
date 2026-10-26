//! HDF5 datatype and dataspace messages, limited to the classes S-102 uses.
use super::io::Cur;
use anyhow::{bail, ensure, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Dt {
    Int {
        size: usize,
        signed: bool,
        big_endian: bool,
    },
    Float {
        size: usize,
        big_endian: bool,
    },
    /// Fixed-size string: padding 0 nullterm, 1 nullpad, 2 spacepad; cset 0 ASCII, 1 UTF-8.
    Str {
        size: usize,
        pad: u8,
        cset: u8,
    },
    Compound {
        size: usize,
        members: Vec<Member>,
    },
    Enum {
        base: Box<Dt>,
        members: Vec<(String, Vec<u8>)>,
    },
    VlenStr {
        cset: u8,
    },
    VlenSeq {
        base: Box<Dt>,
    },
    Array {
        dims: Vec<u32>,
        base: Box<Dt>,
    },
    /// Parsed only far enough to know its size; never decoded.
    Unsupported {
        class: u8,
        size: usize,
    },
}
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub offset: usize,
    pub dt: Dt,
}

impl Dt {
    pub fn size(&self, offset_size: usize) -> usize {
        match self {
            Dt::Int { size, .. }
            | Dt::Float { size, .. }
            | Dt::Str { size, .. }
            | Dt::Compound { size, .. }
            | Dt::Unsupported { size, .. } => *size,
            Dt::Enum { base, .. } => base.size(offset_size),
            // length(4) + global heap collection address + object index(4)
            Dt::VlenStr { .. } | Dt::VlenSeq { .. } => 4 + offset_size + 4,
            Dt::Array { dims, base } => {
                dims.iter().map(|d| *d as usize).product::<usize>() * base.size(offset_size)
            }
        }
    }
}

fn padded_name(c: &mut Cur, pad: bool) -> Result<String> {
    let start = c.p;
    let name = c.cstr()?;
    if pad {
        let used = c.p - start;
        let target = used.div_ceil(8) * 8;
        c.skip(target - used)?;
    }
    Ok(String::from_utf8(name.to_vec())?)
}

pub fn parse(c: &mut Cur, depth: usize) -> Result<Dt> {
    ensure!(depth < 16, "HDF5 datatype nesting too deep");
    let cv = c.u8()?;
    let class = cv & 0x0f;
    let version = cv >> 4;
    ensure!(
        (1..=4).contains(&version),
        "Unsupported HDF5 datatype version {version}"
    );
    let bits = c.bytes(3)?;
    let bits = u32::from(bits[0]) | (u32::from(bits[1]) << 8) | (u32::from(bits[2]) << 16);
    let size = c.u32()? as usize;
    Ok(match class {
        0 => {
            let offset = c.u16()?;
            let precision = c.u16()?;
            ensure!(
                offset == 0 && usize::from(precision) == size * 8 && matches!(size, 1 | 2 | 4 | 8),
                "Unsupported HDF5 integer layout (offset {offset}, precision {precision}, size {size})"
            );
            Dt::Int {
                size,
                signed: bits & 0x08 != 0,
                big_endian: bits & 0x01 != 0,
            }
        }
        1 => {
            ensure!(bits & 0x40 == 0, "VAX floating point is not supported");
            let offset = c.u16()?;
            let precision = c.u16()?;
            let exp_loc = c.u8()?;
            let exp_size = c.u8()?;
            let mant_loc = c.u8()?;
            let mant_size = c.u8()?;
            let bias = c.u32()?;
            let sign = (bits >> 8) & 0xff;
            let ieee = match size {
                4 => {
                    (
                        offset, precision, exp_loc, exp_size, mant_loc, mant_size, bias, sign,
                    ) == (0, 32, 23, 8, 0, 23, 127, 31)
                }
                8 => {
                    (
                        offset, precision, exp_loc, exp_size, mant_loc, mant_size, bias, sign,
                    ) == (0, 64, 52, 11, 0, 52, 1023, 63)
                }
                _ => false,
            };
            if !ieee {
                Dt::Unsupported { class, size }
            } else {
                Dt::Float {
                    size,
                    big_endian: bits & 0x01 != 0,
                }
            }
        }
        3 => Dt::Str {
            size,
            pad: (bits & 0x0f) as u8,
            cset: ((bits >> 4) & 0x0f) as u8,
        },
        6 => {
            let n = (bits & 0xffff) as usize;
            let mut members = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let name = padded_name(c, version < 3)?;
                let offset = if version >= 3 {
                    let width = match size {
                        0..=0xff => 1,
                        0x100..=0xffff => 2,
                        0x1_0000..=0xff_ffff => 3,
                        _ => 4,
                    };
                    c.uint(width)? as usize
                } else {
                    c.u32()? as usize
                };
                let dt = if version == 1 {
                    let rank = c.u8()?;
                    c.skip(3 + 4 + 4)?;
                    let mut dims = Vec::new();
                    for i in 0..4 {
                        let d = c.u32()?;
                        if i < usize::from(rank) {
                            dims.push(d);
                        }
                    }
                    let base = parse(c, depth + 1)?;
                    if rank == 0 {
                        base
                    } else {
                        Dt::Array {
                            dims,
                            base: Box::new(base),
                        }
                    }
                } else {
                    parse(c, depth + 1)?
                };
                members.push(Member { name, offset, dt });
            }
            Dt::Compound { size, members }
        }
        8 => {
            let n = (bits & 0xffff) as usize;
            let base = parse(c, depth + 1)?;
            let mut names = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                names.push(padded_name(c, version < 3)?);
            }
            let width = base.size(8);
            let mut members = Vec::with_capacity(names.len());
            for name in names {
                members.push((name, c.bytes(width)?.to_vec()));
            }
            Dt::Enum {
                base: Box::new(base),
                members,
            }
        }
        9 => {
            let kind = bits & 0x0f;
            let cset = ((bits >> 8) & 0x0f) as u8;
            let base = parse(c, depth + 1)?;
            match kind {
                1 => Dt::VlenStr { cset },
                0 => Dt::VlenSeq {
                    base: Box::new(base),
                },
                _ => bail!("Unsupported HDF5 variable-length kind {kind}"),
            }
        }
        10 => {
            let rank = usize::from(c.u8()?);
            if version < 3 {
                c.skip(3)?;
            }
            let mut dims = Vec::with_capacity(rank);
            for _ in 0..rank {
                dims.push(c.u32()?);
            }
            if version < 3 {
                c.skip(4 * rank)?;
            }
            let base = parse(c, depth + 1)?;
            Dt::Array {
                dims,
                base: Box::new(base),
            }
        }
        2 => {
            c.skip(2)?;
            Dt::Unsupported { class, size }
        }
        4 => {
            c.skip(4)?;
            Dt::Unsupported { class, size }
        }
        5 => {
            let tag = (bits & 0xff) as usize;
            c.skip(tag)?;
            Dt::Unsupported { class, size }
        }
        7 => {
            ensure!(version < 4, "Unsupported HDF5 reference datatype version");
            Dt::Unsupported { class, size }
        }
        _ => bail!("Unknown HDF5 datatype class {class}"),
    })
}

/// Dataspace: `None` dims is the null dataspace; empty dims is scalar.
#[derive(Debug, Clone, PartialEq)]
pub struct Space {
    pub dims: Option<Vec<u64>>,
}
impl Space {
    pub fn count(&self) -> Result<u64> {
        match &self.dims {
            None => Ok(0),
            Some(d) => d.iter().try_fold(1u64, |acc, v| {
                acc.checked_mul(*v)
                    .ok_or_else(|| anyhow::anyhow!("HDF5 dataspace size overflow"))
            }),
        }
    }
}
pub fn parse_space(c: &mut Cur, length_size: usize) -> Result<Space> {
    let version = c.u8()?;
    let rank = usize::from(c.u8()?);
    let flags = c.u8()?;
    ensure!(rank <= 32, "HDF5 dataspace rank {rank} exceeds limit");
    let null = match version {
        1 => {
            c.skip(5)?;
            false
        }
        2 => match c.u8()? {
            0 | 1 => false,
            2 => true,
            t => bail!("Unknown HDF5 dataspace type {t}"),
        },
        _ => bail!("Unsupported HDF5 dataspace version {version}"),
    };
    let mut dims = Vec::with_capacity(rank);
    for _ in 0..rank {
        dims.push(c.uint(length_size)?);
    }
    if flags & 1 != 0 {
        c.skip(rank * length_size)?;
    }
    if version == 1 && flags & 2 != 0 {
        c.skip(rank * length_size)?;
    }
    Ok(Space {
        dims: (!null).then_some(dims),
    })
}
