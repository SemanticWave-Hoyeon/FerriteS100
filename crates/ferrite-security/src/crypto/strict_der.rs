//! Exact DER of ECDSA-Sig-Value, as OpenSSL decides canonical form.
//!
//! OpenSSL parses `SEQUENCE { r INTEGER, s INTEGER }` into unsigned BIGNUMs
//! and re-encodes it; the input is canonical when both encodings are equal.
//! That is exactly: definite minimal lengths, minimal non-negative INTEGER
//! contents (`02 01 00` for zero) and nothing after the SEQUENCE.
use anyhow::{bail, ensure, Result};

/// Read one TLV with tag `tag`; returns (contents, rest). Lengths must be
/// definite and minimally encoded (DER).
fn tlv(input: &[u8], tag: u8) -> Result<(&[u8], &[u8], bool)> {
    let (&found, rest) = input
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("Truncated DER"))?;
    ensure!(found == tag, "Unexpected DER tag");
    let (&first, mut rest) = rest
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("Truncated DER length"))?;
    let mut minimal = true;
    let length = if first < 0x80 {
        first as usize
    } else if first == 0x80 {
        bail!("Indefinite DER length");
    } else {
        let count = (first & 0x7f) as usize;
        ensure!(count <= 4 && rest.len() >= count, "Unsupported DER length");
        let mut length = 0usize;
        for &b in &rest[..count] {
            length = (length << 8) | b as usize;
        }
        if rest[0] == 0 || length < 0x80 {
            minimal = false;
        }
        rest = &rest[count..];
        length
    };
    ensure!(rest.len() >= length, "Truncated DER contents");
    Ok((&rest[..length], &rest[length..], minimal))
}

/// Non-negative minimal INTEGER contents → magnitude (no leading zeros).
fn integer(contents: &[u8]) -> Option<&[u8]> {
    match contents {
        [] => None,
        [0] => Some(&[]),
        [0, next, ..] if next & 0x80 == 0 => None,
        [0, rest @ ..] => Some(rest),
        [first, ..] if first & 0x80 != 0 => None,
        all => Some(all),
    }
}

/// `Ok(Some((r, s)))` for canonical DER (big-endian magnitudes without
/// leading zeros, possibly empty for zero), `Ok(None)` for a decodable but
/// noncanonical encoding, `Err` when the bytes are not an ECDSA-Sig-Value.
pub(crate) fn ecdsa_sig(bytes: &[u8]) -> Result<Option<(&[u8], &[u8])>> {
    let (body, trailing, seq_minimal) = tlv(bytes, 0x30)?;
    let (r, rest, r_minimal) = tlv(body, 0x02)?;
    let (s, rest, s_minimal) = tlv(rest, 0x02)?;
    ensure!(rest.is_empty(), "Unexpected DER in ECDSA signature");
    let canonical = trailing.is_empty() && seq_minimal && r_minimal && s_minimal;
    Ok(match (integer(r), integer(s)) {
        (Some(r), Some(s)) if canonical => Some((r, s)),
        _ => None,
    })
}

pub(crate) fn canonical_ecdsa_der(bytes: &[u8]) -> Result<bool> {
    Ok(ecdsa_sig(bytes)?.is_some())
}
