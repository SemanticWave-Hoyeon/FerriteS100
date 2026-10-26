//! Object headers (v1/v2) and the messages S-102 needs: links, attributes,
//! dataspace, datatype, layout, filters and fill values.
use super::datatype::{self, Dt, Space};
use super::io::{verify_checksum, Cur};
use super::Ctx;
use anyhow::{bail, ensure, Context, Result};

pub const MSG_DATASPACE: u16 = 0x01;
pub const MSG_LINK_INFO: u16 = 0x02;
pub const MSG_DATATYPE: u16 = 0x03;
pub const MSG_FILL_OLD: u16 = 0x04;
pub const MSG_FILL: u16 = 0x05;
pub const MSG_LINK: u16 = 0x06;
pub const MSG_EXTERNAL: u16 = 0x07;
pub const MSG_LAYOUT: u16 = 0x08;
pub const MSG_FILTERS: u16 = 0x0b;
pub const MSG_ATTRIBUTE: u16 = 0x0c;
pub const MSG_CONTINUATION: u16 = 0x10;
pub const MSG_SYMBOL_TABLE: u16 = 0x11;
pub const MSG_ATTRIBUTE_INFO: u16 = 0x15;

/// Message types whose content this reader interprets or may safely ignore.
const KNOWN: &[u16] = &[
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x10, 0x11,
    0x12, 0x13, 0x15, 0x16, 0x17, 0x18,
];
const MAX_MESSAGES: usize = 100_000;
const MAX_BLOCKS: usize = 4096;

#[derive(Debug, Clone)]
pub struct Message {
    pub kind: u16,
    pub flags: u8,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct Header {
    pub messages: Vec<Message>,
}
impl Header {
    pub fn read(ctx: &Ctx, addr: u64) -> Result<Self> {
        let first = ctx.read(addr, 16.min(ctx.len_from(addr)?))?;
        let mut messages = Vec::new();
        let mut pending: Vec<(u64, u64)> = Vec::new();
        let v2 = first.starts_with(b"OHDR");
        let mut creation_order = false;
        if v2 {
            let mut prefix = ctx.read(addr, 6)?;
            ensure!(
                prefix[4] == 2,
                "Unsupported HDF5 object header version {} at {addr}",
                prefix[4]
            );
            let flags = prefix[5];
            creation_order = flags & 0x04 != 0;
            let mut extra = 0;
            if flags & 0x20 != 0 {
                extra += 16;
            }
            if flags & 0x10 != 0 {
                extra += 4;
            }
            let width = 1usize << (flags & 0x03);
            prefix = ctx.read(addr, 6 + extra + width)?;
            let size = super::io::le(&prefix[6 + extra..]) as usize;
            ensure!(size <= 64 * 1024 * 1024, "HDF5 object header too large");
            let start = 6 + extra + width;
            let raw = ctx.read(addr, start + size + 4)?;
            verify_checksum(&raw, "object header")?;
            parse_v2(
                &raw[start..start + size],
                ctx.so,
                ctx.sl,
                creation_order,
                &mut messages,
                &mut pending,
            )?;
        } else {
            let mut c = Cur::new(&first);
            let v = c.u8()?;
            ensure!(
                v == 1,
                "Unsupported HDF5 object header version {v} at {addr} ({:?})",
                &first[..first.len().min(8)]
            );
            c.skip(1)?;
            let _count = c.u16()?;
            c.u32()?;
            let size = c.u32()? as usize;
            ensure!(size <= 64 * 1024 * 1024, "HDF5 object header too large");
            let raw = ctx.read(addr + 16, size)?;
            parse_v1(&raw, ctx.so, ctx.sl, &mut messages, &mut pending)?;
        }
        let mut blocks = 0;
        while let Some((at, len)) = pending.pop() {
            blocks += 1;
            ensure!(
                blocks <= MAX_BLOCKS,
                "HDF5 object header continuation limit"
            );
            ensure!(len <= 64 * 1024 * 1024, "HDF5 continuation block too large");
            let raw = ctx.read(at, len as usize)?;
            if v2 {
                verify_checksum(&raw, "object header continuation")?;
                ensure!(raw.starts_with(b"OCHK"), "Missing HDF5 OCHK signature");
                parse_v2(
                    &raw[4..raw.len() - 4],
                    ctx.so,
                    ctx.sl,
                    creation_order,
                    &mut messages,
                    &mut pending,
                )?;
            } else {
                parse_v1(&raw, ctx.so, ctx.sl, &mut messages, &mut pending)?;
            }
        }
        for m in &messages {
            ensure!(
                m.flags & 0x02 == 0 || m.kind == 0,
                "Shared HDF5 object header messages (type {}) are not supported",
                m.kind
            );
            if !KNOWN.contains(&m.kind) && m.flags & 0x80 != 0 {
                bail!("HDF5 object requires unknown message type {}", m.kind);
            }
        }
        Ok(Self { messages })
    }
    pub fn find(&self, kind: u16) -> Option<&Message> {
        self.messages.iter().find(|m| m.kind == kind)
    }
    pub fn all(&self, kind: u16) -> impl Iterator<Item = &Message> {
        self.messages.iter().filter(move |m| m.kind == kind)
    }
}

fn continuation(data: &[u8], pending: &mut Vec<(u64, u64)>, so: usize, sl: usize) -> Result<()> {
    let mut c = Cur::new(data);
    let at = c.addr(so)?.context("Undefined HDF5 continuation address")?;
    let len = c.uint(sl)?;
    pending.push((at, len));
    Ok(())
}

fn parse_v1(
    raw: &[u8],
    so: usize,
    sl: usize,
    out: &mut Vec<Message>,
    pending: &mut Vec<(u64, u64)>,
) -> Result<()> {
    let mut c = Cur::new(raw);
    while c.remaining() >= 8 {
        let kind = c.u16()?;
        let size = usize::from(c.u16()?);
        let flags = c.u8()?;
        c.skip(3)?;
        let data = c.bytes(size)?.to_vec();
        ensure!(out.len() < MAX_MESSAGES, "Too many HDF5 header messages");
        if kind == MSG_CONTINUATION {
            continuation(&data, pending, so, sl)?;
        }
        out.push(Message { kind, flags, data });
    }
    Ok(())
}

fn parse_v2(
    raw: &[u8],
    so: usize,
    sl: usize,
    creation_order: bool,
    out: &mut Vec<Message>,
    pending: &mut Vec<(u64, u64)>,
) -> Result<()> {
    let head = 4 + if creation_order { 2 } else { 0 };
    let mut c = Cur::new(raw);
    while c.remaining() >= head {
        let kind = u16::from(c.u8()?);
        let size = usize::from(c.u16()?);
        let flags = c.u8()?;
        if creation_order {
            c.skip(2)?;
        }
        let data = c.bytes(size)?.to_vec();
        ensure!(out.len() < MAX_MESSAGES, "Too many HDF5 header messages");
        if kind == MSG_CONTINUATION {
            continuation(&data, pending, so, sl)?;
        }
        out.push(Message { kind, flags, data });
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum LinkTarget {
    Hard(u64),
    Soft(String),
    External,
}

/// A link message (also the payload of dense link heap objects).
pub fn parse_link(data: &[u8], so: usize) -> Result<(String, LinkTarget)> {
    let mut c = Cur::new(data);
    ensure!(c.u8()? == 1, "Unsupported HDF5 link message version");
    let flags = c.u8()?;
    let kind = if flags & 0x08 != 0 { c.u8()? } else { 0 };
    if flags & 0x04 != 0 {
        c.skip(8)?;
    }
    if flags & 0x10 != 0 {
        c.u8()?;
    }
    let len = c.uint(1 << (flags & 0x03))? as usize;
    let name = String::from_utf8(c.bytes(len)?.to_vec()).context("Invalid HDF5 link name")?;
    let target = match kind {
        0 => LinkTarget::Hard(c.addr(so)?.context("Undefined hard link address")?),
        1 => {
            let n = usize::from(c.u16()?);
            LinkTarget::Soft(String::from_utf8_lossy(c.bytes(n)?).into_owned())
        }
        _ => LinkTarget::External,
    };
    Ok((name, target))
}

#[derive(Debug, Clone)]
pub struct AttributeMessage {
    pub name: String,
    pub dt: Dt,
    pub space: Space,
    pub data: Vec<u8>,
}
pub fn parse_attribute(data: &[u8], sl: usize) -> Result<AttributeMessage> {
    let mut c = Cur::new(data);
    let version = c.u8()?;
    let flags = c.u8()?;
    ensure!(
        (1..=3).contains(&version),
        "Unsupported HDF5 attribute version {version}"
    );
    ensure!(
        version == 1 || flags & 0x03 == 0,
        "Shared HDF5 attribute datatypes/dataspaces are not supported"
    );
    let name_len = usize::from(c.u16()?);
    let dt_len = usize::from(c.u16()?);
    let ds_len = usize::from(c.u16()?);
    if version == 3 {
        c.u8()?;
    }
    let pad = |n: usize| if version == 1 { n.div_ceil(8) * 8 } else { n };
    let name_raw = c.bytes(pad(name_len))?;
    let name_raw = &name_raw[..name_len];
    let name = name_raw.strip_suffix(&[0]).unwrap_or(name_raw);
    let name = String::from_utf8(name.to_vec()).context("Invalid HDF5 attribute name")?;
    let dt_raw = c.bytes(pad(dt_len))?;
    let dt = datatype::parse(&mut Cur::new(dt_raw), 0)?;
    let ds_raw = c.bytes(pad(ds_len))?;
    let space = datatype::parse_space(&mut Cur::new(ds_raw), sl)?;
    let rest = &data[c.p..];
    Ok(AttributeMessage {
        name,
        dt,
        space,
        data: rest.to_vec(),
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Filter {
    pub id: u16,
    pub client: [u32; 1],
    pub has_client: bool,
}
pub fn parse_filters(data: &[u8]) -> Result<Vec<Filter>> {
    let mut c = Cur::new(data);
    let version = c.u8()?;
    let n = usize::from(c.u8()?);
    ensure!(n <= 32, "Too many HDF5 filters");
    if version == 1 {
        c.skip(6)?;
    } else {
        ensure!(version == 2, "Unsupported HDF5 filter pipeline version");
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let id = c.u16()?;
        let name_len = if version == 1 || id >= 256 {
            usize::from(c.u16()?)
        } else {
            0
        };
        c.u16()?; // flags (optional filters are applied like mandatory ones)
        let nvalues = usize::from(c.u16()?);
        if name_len > 0 {
            c.skip(if version == 1 {
                name_len.div_ceil(8) * 8
            } else {
                name_len
            })?;
        }
        let mut client = [0u32; 1];
        for i in 0..nvalues {
            let v = c.u32()?;
            if i == 0 {
                client[0] = v;
            }
        }
        if version == 1 && nvalues % 2 == 1 {
            c.skip(4)?;
        }
        out.push(Filter {
            id,
            client,
            has_client: nvalues > 0,
        });
    }
    Ok(out)
}

/// The defined fill value bytes (`None`: library default of zeros).
pub fn parse_fill(header: &Header) -> Result<Option<Vec<u8>>> {
    if let Some(m) = header.find(MSG_FILL) {
        let mut c = Cur::new(&m.data);
        let version = c.u8()?;
        match version {
            1 | 2 => {
                c.skip(2)?;
                let defined = c.u8()?;
                if version == 1 || defined == 1 {
                    if c.remaining() < 4 {
                        return Ok(None);
                    }
                    let size = c.u32()? as usize;
                    return (size > 0)
                        .then(|| c.bytes(size).map(<[u8]>::to_vec))
                        .transpose();
                }
                Ok(None)
            }
            3 => {
                let flags = c.u8()?;
                if flags & 0x20 != 0 {
                    let size = c.u32()? as usize;
                    return (size > 0)
                        .then(|| c.bytes(size).map(<[u8]>::to_vec))
                        .transpose();
                }
                Ok(None)
            }
            _ => bail!("Unsupported HDF5 fill value version {version}"),
        }
    } else if let Some(m) = header.find(MSG_FILL_OLD) {
        let mut c = Cur::new(&m.data);
        let size = c.u32()? as usize;
        Ok((size > 0)
            .then(|| c.bytes(size).map(<[u8]>::to_vec))
            .transpose()?)
    } else {
        Ok(None)
    }
}
