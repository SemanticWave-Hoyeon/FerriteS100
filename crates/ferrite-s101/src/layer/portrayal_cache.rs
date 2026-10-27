//! Per-cell portrayal instruction cache format: magic, schema version and a
//! SHA-256 corruption check around a bincode instruction payload.
use ferrite_product_api::platform as secure_instruction_cache;
use ferrite_product_api::platform::CacheStore;
use ferrite_render::DrawingInstruction;
use sha2::{Digest, Sha256};

/// Cache file format:
/// `[4B magic "FRC\x01"][4B schema version][32B SHA-256 hash][payload]`
///
/// The schema version is incremented whenever `DrawingInstruction` struct
/// layout changes, ensuring stale caches are rejected instead of producing
/// corrupted rendering data.
pub const CACHE_MAGIC: &[u8; 4] = b"FRC\x01";
/// Bump when instruction fields or conversion admission semantics change.
/// Schema 42 rejects older caches that could silently omit augmented geometry.
pub const CACHE_SCHEMA_VERSION: u32 = 43;

/// Wrap a bincode payload with magic + schema version + SHA-256 corruption-detection hash.
/// NOTE: This is NOT cryptographic authentication — it detects accidental corruption only.
pub fn wrap_cache(payload: &[u8]) -> Vec<u8> {
    let hash = Sha256::digest(payload);
    let mut out = Vec::with_capacity(4 + 4 + 32 + payload.len());
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&CACHE_SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&hash);
    out.extend_from_slice(payload);
    out
}

/// Verify integrity and deserialize a cache file.
/// Returns Err if magic/version mismatch or SHA-256 hash doesn't match.
pub fn verify_and_deserialize_cache(
    data: &[u8],
) -> std::result::Result<Vec<ferrite_render::DrawingInstruction>, String> {
    check_and_deserialize_cache(data, true)
}

/// Read entry `path` of `store` and deserialize it like
/// [`verify_and_deserialize_cache`]. The SHA-256 check is skipped when the
/// store vouches that these exact bytes passed it before
/// ([`CacheStore::read_entry`], in-memory stores only); a passing check is
/// recorded for the next read ([`CacheStore::mark_verified`]). Magic and
/// schema version are always checked. Err: the entry could not be read
/// (`Ok(Err(..))`: it was read and rejected).
pub fn read_and_deserialize_cache(
    store: &dyn CacheStore,
    path: &std::path::Path,
) -> std::io::Result<std::result::Result<Vec<DrawingInstruction>, String>> {
    let entry = store.read_entry(path)?;
    let decoded = check_and_deserialize_cache(&entry.bytes, !entry.verified);
    if let (Ok(_), false, Some(version)) = (&decoded, entry.verified, entry.version) {
        store.mark_verified(path, version);
    }
    Ok(decoded)
}

/// The payload SHA-256 stored in a cache file (`[8..40]` after the magic
/// and schema version), None when `data` is no cache file. A host that
/// hashed the payload (`data[40..]`) itself can compare it with this and
/// store the entry as checked ([`ferrite_product_api::platform::MemoryCacheStore::write_verified`]).
pub fn stored_digest(data: &[u8]) -> Option<&[u8]> {
    (data.len() >= 40 && &data[..4] == CACHE_MAGIC).then(|| &data[8..40])
}

fn check_and_deserialize_cache(
    data: &[u8],
    check_digest: bool,
) -> std::result::Result<Vec<ferrite_render::DrawingInstruction>, String> {
    const HEADER_LEN: usize = 4 + 4 + 32; // magic + version + hash
    if data.len() as u64 > secure_instruction_cache::MAX_CACHE_BYTES {
        return Err("cache exceeds byte budget".into());
    }
    if data.len() < HEADER_LEN {
        return Err("cache file too small".into());
    }
    // Check magic
    if &data[..4] != CACHE_MAGIC {
        return Err("invalid cache magic (legacy or corrupted file)".into());
    }
    // Check schema version
    let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
    if version != CACHE_SCHEMA_VERSION {
        return Err(format!(
            "schema version mismatch: file={}, expected={}",
            version, CACHE_SCHEMA_VERSION
        ));
    }
    // Verify SHA-256 hash
    let stored_hash = &data[8..40];
    let payload = &data[HEADER_LEN..];
    if check_digest && Sha256::digest(payload).as_slice() != stored_hash {
        return Err("SHA-256 integrity check failed (file tampered or corrupted)".into());
    }
    decode_cache_payload(payload)
}

/// Payloads are one bincode instruction sequence, or (for larger cells)
/// `SEGMENTED_CACHE`, a segment count, the segments' byte lengths and the
/// segments, each such a sequence, decoded in parallel and concatenated.
pub const SEGMENTED_CACHE: u64 = u64::MAX - 1;
pub const CACHE_SEGMENT_INSTRUCTIONS: usize = 8192;
pub const MAX_CACHE_SEGMENTS: u64 = 1 << 16;

pub fn encode_cache_payload(
    instructions: &[DrawingInstruction],
) -> std::result::Result<Vec<u8>, String> {
    use bincode::Options;
    use ferrite_kernel::par::prelude::*;
    let options = || {
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(secure_instruction_cache::MAX_CACHE_BYTES - 40)
    };
    if instructions.len() <= CACHE_SEGMENT_INSTRUCTIONS {
        return options().serialize(instructions).map_err(|e| e.to_string());
    }
    let segments = instructions
        .par_chunks(CACHE_SEGMENT_INSTRUCTIONS)
        .map(|chunk| options().serialize(chunk).map_err(|e| e.to_string()))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let total = 16 + 8 * segments.len() + segments.iter().map(Vec::len).sum::<usize>();
    if total as u64 > secure_instruction_cache::MAX_CACHE_BYTES - 40 {
        return Err("cache exceeds byte budget".into());
    }
    let mut payload = Vec::with_capacity(total);
    payload.extend_from_slice(&SEGMENTED_CACHE.to_le_bytes());
    payload.extend_from_slice(&(segments.len() as u64).to_le_bytes());
    for segment in &segments {
        payload.extend_from_slice(&(segment.len() as u64).to_le_bytes());
    }
    for segment in &segments {
        payload.extend_from_slice(segment);
    }
    Ok(payload)
}

pub fn decode_cache_payload(
    payload: &[u8],
) -> std::result::Result<Vec<ferrite_render::DrawingInstruction>, String> {
    let word = |bytes: &[u8], at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    // Reject byte/declared top-level count budgets before Vec allocation.
    // Nested collection allocation is constrained by the encoded byte budget,
    // not an exact heap cap; this disposable cache is app-private, not input data.
    let first = word(payload, 0)
        .ok_or_else(|| "deserialization failed: missing instruction count".to_owned())?;
    let decode = |bytes: &[u8]| -> std::result::Result<Vec<DrawingInstruction>, String> {
        use bincode::Options;
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(secure_instruction_cache::MAX_CACHE_BYTES)
            .reject_trailing_bytes()
            .deserialize(bytes)
            .map_err(|e| format!("deserialization failed: {}", e))
    };
    if first != SEGMENTED_CACHE {
        if first > secure_instruction_cache::MAX_INSTRUCTIONS {
            return Err("cache exceeds instruction budget".into());
        }
        return decode(payload);
    }
    let malformed = || "deserialization failed: malformed segment table".to_owned();
    let count = word(payload, 8).ok_or_else(malformed)?;
    if count == 0 || count > MAX_CACHE_SEGMENTS {
        return Err(malformed());
    }
    let mut segments = Vec::with_capacity(count as usize);
    let mut offset = 16 + 8 * count as usize;
    let mut instructions = 0u64;
    for index in 0..count as usize {
        let length = word(payload, 16 + 8 * index).ok_or_else(malformed)? as usize;
        let end = offset.checked_add(length).ok_or_else(malformed)?;
        let segment = payload.get(offset..end).ok_or_else(malformed)?;
        let declared = word(segment, 0)
            .ok_or_else(|| "deserialization failed: missing instruction count".to_owned())?;
        instructions = instructions.saturating_add(declared);
        if instructions > secure_instruction_cache::MAX_INSTRUCTIONS {
            return Err("cache exceeds instruction budget".into());
        }
        segments.push(segment);
        offset = end;
    }
    if offset != payload.len() {
        return Err(malformed());
    }
    use ferrite_kernel::par::prelude::*;
    let decoded: Vec<_> = segments.par_iter().map(|segment| decode(segment)).collect();
    let mut all = Vec::with_capacity(instructions as usize);
    for segment in decoded {
        all.extend(segment?);
    }
    Ok(all)
}

#[cfg(test)]
mod instruction_cache_schema_tests {
    use super::*;
    use ferrite_render::{AreaInstruction, DrawingInstruction, PatternCrs, WorldPoint};

    #[test]
    fn cache_rejects_forged_sequence_count_before_deserialize() {
        let file = wrap_cache(&u64::MAX.to_le_bytes());
        assert_eq!(
            verify_and_deserialize_cache(&file).unwrap_err(),
            "cache exceeds instruction budget"
        );
        let mut payload = bincode::serialize(&Vec::<DrawingInstruction>::new()).unwrap();
        payload.push(0);
        assert!(verify_and_deserialize_cache(&wrap_cache(&payload))
            .unwrap_err()
            .contains("deserialization failed"));
    }

    /// Large cells are written as segments decoded in parallel; the result
    /// equals the input, and every count/length is checked before decoding.
    #[test]
    fn segmented_payload_round_trips_and_rejects_bad_tables() {
        use ferrite_render::PointInstruction;
        let instructions: Vec<_> = (0..20_000)
            .map(|i| {
                DrawingInstruction::Point(
                    PointInstruction::new(format!("S{i}"), WorldPoint::new(i as f64, 1.))
                        .with_cell_index(i % 7),
                )
            })
            .collect();
        let payload = encode_cache_payload(&instructions).unwrap();
        assert_eq!(&payload[..8], &SEGMENTED_CACHE.to_le_bytes());
        let decoded = verify_and_deserialize_cache(&wrap_cache(&payload)).unwrap();
        assert_eq!(
            bincode::serialize(&decoded).unwrap(),
            bincode::serialize(&instructions).unwrap()
        );
        // Small cells keep the single-sequence payload.
        let small = encode_cache_payload(&instructions[..10]).unwrap();
        assert_eq!(small, bincode::serialize(&instructions[..10]).unwrap());
        // A truncated table, a wrong length and an excessive count all fail.
        let decode = |p: &[u8]| verify_and_deserialize_cache(&wrap_cache(p));
        assert!(decode(&payload[..20])
            .unwrap_err()
            .contains("segment table"));
        let mut longer = payload.clone();
        longer.push(0);
        assert!(decode(&longer).unwrap_err().contains("segment table"));
        let mut forged = payload.clone();
        let first_segment =
            16 + 8 * u64::from_le_bytes(payload[8..16].try_into().unwrap()) as usize;
        forged[first_segment..first_segment + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            decode(&forged).unwrap_err(),
            "cache exceeds instruction budget"
        );
    }

    /// Schema 43 adds stored area triangulations; bincode is not
    /// self-describing, so schema-42 caches must never be decoded.
    #[test]
    fn schema43_rejects_caches_without_stored_triangulations() {
        let mut wrapped = wrap_cache(&[]);
        wrapped[4..8].copy_from_slice(&42u32.to_le_bytes());
        assert!(verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("schema version mismatch: file=42, expected=43"));
    }
    #[test]
    fn schema42_rejects_geometry_omission_cache_before_hash_or_decode() {
        let mut wrapped = wrap_cache(&[]);
        wrapped[4..8].copy_from_slice(&41u32.to_le_bytes());
        wrapped[8] ^= 1;
        assert!(verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("schema version mismatch: file=41, expected=43"));
    }

    #[test]
    fn schema42_retains_geographic_annulus_and_rejects_incomplete_old_portrayal_cache() {
        let mut line = ferrite_render::LineInstruction::new(vec![
            WorldPoint::new(179.9, 50.),
            WorldPoint::new(179.9, 50.),
        ]);
        line.portrayal_path = Some(ferrite_render::PortrayalPath::GeographicAnnulus {
            center: (179.9, 50.),
            outer: 50_000.,
            inner: 25_000.,
            start: 35.,
            sweep: -270.,
        });
        let payload = bincode::serialize(&vec![DrawingInstruction::Line(line)]).unwrap();
        let wrapped = wrap_cache(&payload);
        let restored = verify_and_deserialize_cache(&wrapped).unwrap();
        assert_eq!(bincode::serialize(&restored).unwrap(), payload);
        let mut old = wrapped;
        old[4..8].copy_from_slice(&40u32.to_le_bytes());
        assert!(verify_and_deserialize_cache(&old)
            .unwrap_err()
            .contains("file=40, expected=43"));
    }
    #[test]
    fn schema42_rejects_prior_unscoped_optional_policy_cache_before_decode() {
        let mut wrapped =
            wrap_cache(&bincode::serialize(&Vec::<DrawingInstruction>::new()).unwrap());
        wrapped[4..8].copy_from_slice(&39u32.to_le_bytes());
        assert!(verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("schema version mismatch: file=39, expected=43"));
    }

    #[test]
    fn schema42_keeps_full_text_font_style_and_rejects_schema38_before_decode() {
        let mut text =
            ferrite_render::TextInstruction::new("depth".into(), WorldPoint::new(1., 2.));
        text.font_style = ferrite_render::TextFontStyle {
            weight: ferrite_render::TextFontWeight::Light,
            proportion: ferrite_render::TextFontProportion::MonoSpaced,
            serifs: true,
            underline: true,
            strikethrough: true,
            upperline: true,
            reference: Some("Font-A".into()),
        };
        let style = text.font_style.clone();
        let mut wrapped = wrap_cache(
            &bincode::serialize(&vec![DrawingInstruction::Text(Box::new(text))]).unwrap(),
        );
        let restored = verify_and_deserialize_cache(&wrapped).unwrap();
        let DrawingInstruction::Text(text) = &restored[0] else {
            panic!()
        };
        assert_eq!(text.font_style, style);
        wrapped[4..8].copy_from_slice(&38u32.to_le_bytes());
        assert!(verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("schema version mismatch: file=38, expected=43"));
    }
    #[test]
    fn schema42_retains_authored_fill_reference_and_rejects_37() {
        let mut area = AreaInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 0.),
            WorldPoint::new(0., 1.),
        ])
        .with_pattern_fill("DIAMOND1P".into(), (22.5, 0.), (0., 43.13));
        area.fill_ref = Some("DIAMOND1".into());
        let mut wrapped = wrap_cache(
            &bincode::serialize(&vec![DrawingInstruction::Area(Box::new(area))]).unwrap(),
        );
        let decoded = verify_and_deserialize_cache(&wrapped).unwrap();
        let DrawingInstruction::Area(area) = &decoded[0] else {
            panic!("not area")
        };
        assert_eq!(area.fill_ref.as_deref(), Some("DIAMOND1"));
        wrapped[4..8].copy_from_slice(&37u32.to_le_bytes());
        assert!(verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("file=37, expected=43"));
    }

    #[test]
    fn schema42_round_trip_preserves_both_clip_modes_and_source() {
        let instructions: Vec<_> = [true, false]
            .into_iter()
            .map(|clip| {
                DrawingInstruction::Area(Box::new(
                    AreaInstruction::new(vec![
                        WorldPoint::new(0., 0.),
                        WorldPoint::new(1., 0.),
                        WorldPoint::new(1., 1.),
                        WorldPoint::new(0., 0.),
                    ])
                    .with_pattern_fill("motif".into(), (-2., 1.), (1., 3.))
                    .with_pattern_crs(PatternCrs::LocalGeometry)
                    .with_pattern_clip_symbols(clip)
                    .with_feature_id(42)
                    .with_cell_index(3),
                ))
            })
            .collect();
        let payload = bincode::serialize(&instructions).unwrap();
        let file = wrap_cache(&payload);
        assert_eq!(&file[4..8], &CACHE_SCHEMA_VERSION.to_le_bytes());
        let decoded = verify_and_deserialize_cache(&file).unwrap();
        assert_eq!(bincode::serialize(&decoded).unwrap(), payload);
        for (instruction, clip) in decoded.iter().zip([true, false]) {
            let DrawingInstruction::Area(area) = instruction else {
                panic!("not area")
            };
            assert_eq!(area.pattern_clip_symbols, clip);
            assert_eq!(area.pattern_crs, PatternCrs::LocalGeometry);
            assert_eq!(instruction.cell_index(), Some(3));
            assert_eq!(instruction.feature_id(), Some(42));
        }
    }

    #[test]
    fn rejects_legacy_schema_before_hash_or_decode_and_rejects_bad_headers() {
        let mut file = wrap_cache(&[1, 0, 0, 0, 0, 0, 0, 0]);
        file[4..8].copy_from_slice(&36u32.to_le_bytes());
        file[8] ^= 1; // Invalid hash must not bypass the schema gate.
        assert_eq!(
            verify_and_deserialize_cache(&file).unwrap_err(),
            "schema version mismatch: file=36, expected=43"
        );
        assert_eq!(
            verify_and_deserialize_cache(&file[..39]).unwrap_err(),
            "cache file too small"
        );
        file[0] ^= 1;
        assert!(verify_and_deserialize_cache(&file)
            .unwrap_err()
            .contains("magic"));
    }

    #[test]
    fn current_schema_checks_corruption_before_deserialization() {
        let payload = bincode::serialize(&Vec::<DrawingInstruction>::new()).unwrap();
        let mut file = wrap_cache(&payload);
        file[40] ^= 1;
        assert!(verify_and_deserialize_cache(&file)
            .unwrap_err()
            .contains("SHA-256"));
        let valid_hash_bad_payload = wrap_cache(&[1, 0, 0, 0, 0, 0, 0, 0]);
        assert!(verify_and_deserialize_cache(&valid_hash_bad_payload)
            .unwrap_err()
            .contains("deserialization failed"));
    }

    #[test]
    #[ignore = "requires separately SHA-guarded actual schema36 and schema37 SHOM payloads"]
    fn actual_shom_payloads_reject_36_and_round_trip_37() {
        let legacy = std::fs::read(std::env::var("FERRITE_CACHE36_PAYLOAD").unwrap()).unwrap();
        let mut old_file = wrap_cache(&legacy);
        old_file[4..8].copy_from_slice(&36u32.to_le_bytes());
        assert_eq!(
            verify_and_deserialize_cache(&old_file).unwrap_err(),
            "schema version mismatch: file=36, expected=43"
        );
        let current = std::fs::read(std::env::var("FERRITE_CACHE37_PAYLOAD").unwrap()).unwrap();
        let decoded = verify_and_deserialize_cache(&wrap_cache(&current)).unwrap();
        assert_eq!(decoded.len(), 69_570);
        assert_eq!(bincode::serialize(&decoded).unwrap(), current);
        let pattern_count = decoded
            .iter()
            .filter(|instruction| {
                matches!(instruction, DrawingInstruction::Area(a)
                if matches!(a.fill, ferrite_render::AreaFillType::Pattern { .. }))
            })
            .count();
        assert_eq!(pattern_count, 5_375);
        println!(
            "actual cache37: {} instructions / {} patterns; legacy36 rejected",
            decoded.len(),
            pattern_count
        );
    }
}
