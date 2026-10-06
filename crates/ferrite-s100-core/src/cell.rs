//! S-101 Cell container
//!
//! Represents a complete S-101 ENC cell with all records.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ferrite_iso8211::{
    read_string, tags, Iso8211Parser, MmapIso8211Parser, RawField, DR, FIELD_TERMINATOR,
};

use crate::{
    Attribute, CodeMapping, CompositeCurveRecord, Coordinate, CurveRecord, CurveSegment,
    DatasetCodeMappings, FeatureAssociation, FeatureRecord, InformationAssociation,
    InformationRecord, MaskRecord, MultiPointRecord, OrientedCurve, PointRecord, RecordId, Result,
    S100Error, SegmentType, SpatialAssociation, SpatialPrimitiveType, SurfaceRecord, FOID, FRID,
    IRID,
};

/// Dataset identification
#[derive(Debug, Clone, Default)]
pub struct DatasetIdentification {
    pub dataset_name: String,
    pub dataset_title: String,
    pub product_identifier: String,
    pub product_edition: String,
    pub edition_number: u16,
    pub update_number: u16,
    pub application_profile: String,
    pub update_application_date: String,
    pub issue_date: String,
}

/// Digest of the exact retained input bytes used to parse one cell.
/// Authentication policy is owned separately by the security/application layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellSourceIdentity {
    sha256: [u8; 32],
}
impl CellSourceIdentity {
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

/// S-101 Cell container
#[derive(Debug)]
pub struct S101Cell {
    /// Source file path
    pub file_path: PathBuf,
    /// Dataset identification
    pub dsid: DatasetIdentification,
    /// Code mappings
    pub code_mappings: DatasetCodeMappings,
    /// Coordinate multiplication factor (1/CMFX)
    pub coord_factor: f64,
    pub coord_factor_y: f64,
    /// Coordinate multiplication factor for Z (1/CMFZ)
    pub coord_factor_z: f64,
    /// Coordinate origin X (DCOX)
    pub coord_origin_x: f64,
    /// Coordinate origin Y (DCOY)
    pub coord_origin_y: f64,
    pub coord_origin_z: f64,
    // === S-101 Scale Information ===
    /// Minimum display scale (smallest scale = largest denominator)
    /// Features may not be shown at scales smaller than this
    pub minimum_display_scale: Option<u32>,
    /// Maximum display scale (largest scale = smallest denominator)
    /// Overscale warning shown when viewing at scales larger than this
    pub maximum_display_scale: Option<u32>,
    /// Point records
    pub points: HashMap<i64, PointRecord>,
    /// Multi-point records
    pub multi_points: HashMap<i64, MultiPointRecord>,
    /// Curve records
    pub curves: HashMap<i64, CurveRecord>,
    /// Composite curve records
    pub composite_curves: HashMap<i64, CompositeCurveRecord>,
    /// Surface records
    pub surfaces: HashMap<i64, SurfaceRecord>,
    /// Feature records
    pub features: HashMap<i64, FeatureRecord>,
    /// Information records
    pub information: HashMap<i64, InformationRecord>,
    /// Spatial-record information associations retained for quality/provenance.
    pub spatial_information_associations: HashMap<i64, Vec<InformationAssociation>>,
}

impl S101Cell {
    /// Materialize a sequential S-101 update chain. Currently ordered spatial
    /// controls, attribute trees and supported association updates are applied.
    /// Unsupported product/profile fields return an error; no cell
    /// is published. Authentication is a separate prerequisite for the caller.
    /// Reissue bases (profile 1 with a nonzero filename extension) are allowed.
    pub fn load_with_spatial_updates(base: &Path, updates: &[PathBuf]) -> Result<Self> {
        Self::load_update_chain_from_with_identity(base, base, updates).map(|(cell, _)| cell)
    }

    /// Load private retained inputs, preserving the original source path. Empty
    /// chains retain legacy base parsing and its exact raw-byte identity. Nonempty
    /// identities commit to ordered input lengths/digests, not reconstructed data.
    /// Authentication remains the caller's separate prerequisite.
    pub fn load_update_chain_from_with_identity(
        source: &Path,
        base_input: &Path,
        updates: &[PathBuf],
    ) -> Result<(Self, CellSourceIdentity)> {
        use crate::updates::{S100RecordStore, UpdateLimits};
        use sha2::Digest;
        let limits = UpdateLimits::default();
        if updates.is_empty() {
            return Self::parse_owned_with_identity(
                source,
                bounded_update_parser(base_input, limits.max_dataset_bytes)?,
            );
        }
        if updates.len() > 999 {
            return Err(S100Error::InvalidRecord(
                "Update-chain input count budget exceeded".into(),
            ));
        }
        let mut chain = sha2::Sha256::new();
        chain.update(b"FerriteS100/S101/ordered-raw-update-chain/v1\0");
        chain.update((updates.len() as u64 + 1).to_le_bytes());
        let mut total_bytes = 0usize;
        let mut parser = bounded_update_parser(base_input, limits.max_dataset_bytes)?;
        hash_chain_input(
            &mut chain,
            0,
            parser.remaining(),
            &mut total_bytes,
            limits.max_dataset_bytes,
        )?;
        let (_, base_records) = parser.read_all()?;
        drop(parser);
        let id = records_identification(&base_records)?;
        if id.application_profile != "1" {
            return Err(S100Error::InvalidRecord(
                "Update chain needs profile-1 base".into(),
            ));
        }
        if id.product_identifier != "INT.IHO.S-101.2.0"
            || !matches!(id.product_edition.as_str(), "2.0" | "2.0.0")
        {
            return Err(S100Error::InvalidRecord(
                "Spatial update adapter requires S-101 edition 2.0".into(),
            ));
        }
        let (stem, mut number) = dataset_filename(&id.dataset_name)?;
        if id.edition_number == 0
            || (records_dsed(&base_records)?.contains('.') && id.update_number != number)
        {
            return Err(S100Error::InvalidRecord(
                "Base edition/update identity mismatch".into(),
            ));
        }
        let mut base_metadata: Vec<DR> = base_records
            .iter()
            .filter(|dr| {
                dr.fields.first().is_some_and(|f| {
                    crate::updates::UpdateRecordHeader::parse(f).is_ok_and(|h| h.is_none())
                })
            })
            .cloned()
            .collect();
        validate_update_metadata(&base_metadata, &base_metadata)?;
        let mut store = S100RecordStore::from_base(base_records, limits)?;
        for (index, path) in updates.iter().enumerate() {
            let mut parser = bounded_update_parser(path, limits.max_dataset_bytes)?;
            hash_chain_input(
                &mut chain,
                index + 1,
                parser.remaining(),
                &mut total_bytes,
                limits.max_dataset_bytes,
            )?;
            let (_, mut records) = parser.read_all()?;
            drop(parser);
            let next = records_identification(&records)?;
            let (next_stem, next_number) = dataset_filename(&next.dataset_name)?;
            if next.application_profile != "2"
                || stem != next_stem
                || number.checked_add(1) != Some(next_number)
                || next.update_number != next_number
                || next.edition_number != id.edition_number
                || next.product_identifier != id.product_identifier
                || next.product_edition != id.product_edition
            {
                return Err(S100Error::InvalidRecord(
                    "Dataset update identity/order mismatch".into(),
                ));
            }
            validate_update_metadata(&base_metadata, &records)?;
            let dictionary_changed = extend_update_dictionary(&mut base_metadata, &records)?;
            remap_update_codes(&base_metadata, &mut records)?;
            if dictionary_changed {
                store.replace_code_metadata(base_metadata.clone())?;
            }
            let mut data = Vec::new();
            for dr in records {
                if let Some(f) = dr.fields.first() {
                    if crate::updates::UpdateRecordHeader::parse(f)?.is_some() {
                        data.push(dr);
                    }
                }
            }
            store.apply_records(&data)?;
            number = next_number;
        }
        store.validate_references()?;
        store.validate_s101_feature_ids()?;
        store.validate_s101_attribute_trees()?;
        let mut cell = Self::parse_records_with_graph_check(source, store.into_records(), true)?;
        cell.dsid.update_number = number;
        Ok((
            cell,
            CellSourceIdentity {
                sha256: chain.finalize().into(),
            },
        ))
    }
    /// Load cell from file using memory-mapped I/O (zero-copy, 3-10x faster)
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::load_from(path.as_ref(), path.as_ref())
    }

    /// Parse a retained private snapshot while preserving the original dataset identity.
    pub fn load_from(source: &Path, data_path: &Path) -> Result<Self> {
        Self::parse_mapped(source, MmapIso8211Parser::from_file(data_path)?)
    }

    /// Capture a cell's input once, then hash and parse the same owned bytes.
    /// Later edits or truncation of data_path cannot change this parser's input.
    /// This is a content identity, not an authentication or atomic-file-read claim.
    /// The temporary input buffer is released before building cell dictionaries.
    pub fn load_from_with_identity(
        source: &Path,
        data_path: &Path,
    ) -> Result<(Self, CellSourceIdentity)> {
        Self::parse_owned_with_identity(source, Iso8211Parser::from_file(data_path)?)
    }

    fn parse_owned_with_identity(
        source: &Path,
        mut parser: Iso8211Parser,
    ) -> Result<(Self, CellSourceIdentity)> {
        use sha2::Digest;
        tracing::info!("Loading S-101 cell (owned input): {}", source.display());
        let identity = CellSourceIdentity {
            sha256: sha2::Sha256::digest(parser.remaining()).into(),
        };
        let (_ddr, records) = parser.read_all()?;
        drop(parser);
        let cell = Self::parse_records(source, records)?;
        Ok((cell, identity))
    }

    fn parse_mapped(source: &Path, mut parser: MmapIso8211Parser) -> Result<Self> {
        let path = source;
        tracing::info!("Loading S-101 cell (mmap): {}", path.display());
        let (_ddr, records) = parser.read_all()?;
        Self::parse_records(source, records)
    }

    fn parse_records(source: &Path, records: Vec<DR>) -> Result<Self> {
        Self::parse_records_with_graph_check(source, records, false)
    }
    fn parse_records_with_graph_check(
        source: &Path,
        records: Vec<DR>,
        check_graph: bool,
    ) -> Result<Self> {
        let path = source;
        let mut cell = S101Cell {
            file_path: path.to_path_buf(),
            dsid: DatasetIdentification::default(),
            code_mappings: DatasetCodeMappings::new(),
            coord_factor: 1.0,
            coord_factor_y: 1.0,
            coord_factor_z: 0.01, // Default CMFZ=100, so factor = 1/100
            coord_origin_x: 0.0,
            coord_origin_y: 0.0,
            coord_origin_z: 0.0,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: HashMap::new(),
            multi_points: HashMap::new(),
            curves: HashMap::new(),
            composite_curves: HashMap::new(),
            surfaces: HashMap::new(),
            features: HashMap::new(),
            information: HashMap::new(),
            spatial_information_associations: HashMap::new(),
        };

        // Process records
        for dr in records {
            if dr
                .fields
                .iter()
                .any(|f| matches!(f.tag.as_str(), "COCC" | "SECC" | "CCOC"))
            {
                return Err(S100Error::InvalidRecord(
                    "Update controls require base/update chain application".into(),
                ));
            }
            if let Some(f) = dr.fields.first() {
                if let Some(h) = crate::updates::UpdateRecordHeader::parse(f)? {
                    if h.instruction != ferrite_kernel::sequence_update::UpdateInstruction::Insert {
                        return Err(S100Error::InvalidRecord(
                            "Update record cannot be loaded as a base cell".into(),
                        ));
                    }
                }
            }
            cell.process_record(&dr)?;
        }

        // Apply code mappings to records
        cell.apply_code_mappings();

        if check_graph {
            cell.validate_record_graph()?;
        }

        // Separate interior rings by connectivity (S-100 10a-7.2.6)
        // The RIAS parser collects all interior curves into a single Vec,
        // but they may form multiple disjoint closed rings (holes).
        cell.separate_interior_rings();

        // Shrink excess memory after loading is complete
        cell.shrink_to_fit();

        tracing::info!(
            "Loaded cell: {} features, {} points, {} multi-points, {} curves, {} surfaces",
            cell.features.len(),
            cell.points.len(),
            cell.multi_points.len(),
            cell.curves.len(),
            cell.surfaces.len()
        );

        Ok(cell)
    }

    /// Validate local target existence and composite acyclicity before publishing
    /// a materialized update result. This is not a polygon topology certificate.
    fn validate_record_graph(&self) -> Result<()> {
        let spatial = |id: RecordId| -> bool {
            match id.rcnm {
                110 => self.points.contains_key(&id.key()),
                115 => self.multi_points.contains_key(&id.key()),
                120 => self.curves.contains_key(&id.key()),
                125 => self.composite_curves.contains_key(&id.key()),
                130 => self.surfaces.contains_key(&id.key()),
                _ => false,
            }
        };
        let require = |ok: bool| -> Result<()> {
            if ok {
                Ok(())
            } else {
                Err(S100Error::InvalidRecord(
                    "Dangling or invalid update-result reference".into(),
                ))
            }
        };
        for associations in self.spatial_information_associations.values() {
            for association in associations {
                require(
                    association.info_id.rcnm == 150
                        && self.information.contains_key(&association.info_id.key()),
                )?;
            }
        }
        for curve in self.curves.values() {
            for id in [curve.start_point, curve.end_point].into_iter().flatten() {
                require(id.rcnm == 110 && spatial(id))?;
            }
        }
        for composite in self.composite_curves.values() {
            for c in &composite.curves {
                require(matches!(c.curve_id.rcnm, 120 | 125) && spatial(c.curve_id))?;
            }
        }
        for surface in self.surfaces.values() {
            for c in surface
                .exterior_ring
                .iter()
                .chain(surface.interior_rings.iter().flatten())
            {
                require(matches!(c.curve_id.rcnm, 120 | 125) && spatial(c.curve_id))?;
            }
        }
        for feature in self.features.values() {
            for a in &feature.spatial_associations {
                require(spatial(a.spatial_id))?;
            }
            for a in &feature.masks {
                require(spatial(a.spatial_id))?;
            }
            for a in &feature.information_associations {
                require(a.info_id.rcnm == 150 && self.information.contains_key(&a.info_id.key()))?;
            }
            for a in &feature.feature_associations {
                require(
                    a.feature_id.rcnm == 100 && self.features.contains_key(&a.feature_id.key()),
                )?;
            }
        }
        for info in self.information.values() {
            for a in &info.information_associations {
                require(a.info_id.rcnm == 150 && self.information.contains_key(&a.info_id.key()))?;
            }
        }
        // Iterative DFS, O(records + references), no call-stack depth dependency.
        let mut state = HashMap::<i64, u8>::new();
        for &root in self.composite_curves.keys() {
            if state.get(&root) == Some(&2) {
                continue;
            }
            let mut stack = vec![(root, false)];
            while let Some((key, exit)) = stack.pop() {
                if exit {
                    state.insert(key, 2);
                    continue;
                }
                match state.get(&key) {
                    Some(1) => {
                        return Err(S100Error::InvalidRecord(
                            "Cyclic composite curve in update result".into(),
                        ))
                    }
                    Some(2) => continue,
                    _ => {}
                }
                state.insert(key, 1);
                stack.push((key, true));
                if let Some(c) = self.composite_curves.get(&key) {
                    for child in c.curves.iter().rev().filter(|c| c.curve_id.rcnm == 125) {
                        stack.push((child.curve_id.key(), false));
                    }
                }
            }
        }
        Ok(())
    }

    /// Process a single data record
    fn process_record(&mut self, dr: &DR) -> Result<()> {
        // Determine record type by first field tag
        if let Some(first_field) = dr.fields.first() {
            if matches!(
                first_field.tag.as_str(),
                "PRID" | "MRID" | "CRID" | "CCID" | "SRID"
            ) {
                if let Some(header) = crate::updates::UpdateRecordHeader::parse(first_field)? {
                    let mut associations = Vec::new();
                    for field in dr.find_fields(tags::INAS) {
                        self.parse_info_associations(&field.data, &mut associations)?;
                    }
                    if !associations.is_empty() {
                        self.spatial_information_associations.insert(
                            RecordId::new(header.key.name, header.key.id).key(),
                            associations,
                        );
                    }
                }
            }
            match first_field.tag.as_str() {
                tags::DSID => self.process_dsid(dr)?,
                tags::ATCS | tags::ITCS | tags::FTCS | tags::IACS | tags::FACS | tags::ARCS => {
                    self.process_code_mapping(dr)?
                }
                tags::PRID => self.process_point(dr)?,
                tags::MRID => self.process_multi_point(dr)?,
                tags::CRID => self.process_curve(dr)?,
                tags::CCID => self.process_composite_curve(dr)?,
                tags::SRID => self.process_surface(dr)?,
                tags::FRID => self.process_feature(dr)?,
                tags::IRID => self.process_information(dr)?,
                _ => {
                    tracing::trace!("Skipping record with tag: {}", first_field.tag);
                }
            }
        }
        Ok(())
    }

    /// Process DSID record
    fn process_dsid(&mut self, dr: &DR) -> Result<()> {
        if let Some(field) = dr.find_field(tags::DSID) {
            self.dsid = parse_dataset_identification(field.data_trimmed())?;
            if self.dsid.application_profile == "2" {
                return Err(S100Error::InvalidRecord(
                    "Profile-2 dataset requires a base/update chain".into(),
                ));
            }
            tracing::debug!("DSID record processed");
        }

        // Parse DSSI (Dataset Structure Information) for coordinate parameters
        if let Some(dssi_field) = dr.find_field(tags::DSSI) {
            let data = dssi_field.data_trimmed();
            // DSSI format:
            // DCOX (b48/8 bytes) - offset 0  - Dataset Coordinate Origin X
            // DCOY (b48/8 bytes) - offset 8  - Dataset Coordinate Origin Y
            // DCOZ (b48/8 bytes) - offset 16 - Dataset Coordinate Origin Z
            // CMFX (b14/4 bytes) - offset 24 - Coordinate Multiplication Factor X
            // CMFY (b14/4 bytes) - offset 28 - Coordinate Multiplication Factor Y
            // CMFZ (b14/4 bytes) - offset 32 - Coordinate Multiplication Factor Z

            if data.len() >= 32 {
                // Read coordinate origins (f64, little-endian)
                let dcox = f64::from_le_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ]);
                let dcoy = f64::from_le_bytes([
                    data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
                ]);

                self.coord_origin_x = dcox;
                self.coord_origin_y = dcoy;
                self.coord_origin_z = f64::from_le_bytes(data[16..24].try_into().unwrap());

                // Read multiplication factors
                let cmfx = i32::from_le_bytes([data[24], data[25], data[26], data[27]]);
                let cmfy = i32::from_le_bytes([data[28], data[29], data[30], data[31]]);
                let cmfz = if data.len() >= 36 {
                    i32::from_le_bytes([data[32], data[33], data[34], data[35]])
                } else {
                    100 // Default CMFZ = 100
                };

                if cmfx > 0 {
                    self.coord_factor = 1.0 / (cmfx as f64);
                }
                if cmfy > 0 {
                    self.coord_factor_y = 1.0 / cmfy as f64;
                }
                if cmfz > 0 {
                    self.coord_factor_z = 1.0 / (cmfz as f64);
                }

                tracing::debug!(
                    "DSSI: DCOX={:.6}, DCOY={:.6}, CMFX={}, CMFY={}, CMFZ={}, coord_factor={}, coord_factor_z={}",
                    dcox, dcoy, cmfx, cmfy, cmfz, self.coord_factor, self.coord_factor_z
                );
            }
        }

        // DSID record contains code mapping fields (ATCS, ITCS, FTCS, etc.)
        self.process_code_mapping(dr)?;

        Ok(())
    }

    /// Process code mapping record
    fn process_code_mapping(&mut self, dr: &DR) -> Result<()> {
        for field in &dr.fields {
            let mapping = match field.tag.as_str() {
                tags::ATCS => &mut self.code_mappings.attributes,
                tags::ITCS => &mut self.code_mappings.information_types,
                tags::FTCS => &mut self.code_mappings.feature_types,
                tags::IACS => &mut self.code_mappings.information_associations,
                tags::FACS => &mut self.code_mappings.feature_associations,
                tags::ARCS => &mut self.code_mappings.association_roles,
                _ => continue,
            };

            Self::parse_code_field(&field.data, mapping)?;
        }
        Ok(())
    }

    /// Parse code mapping field
    /// S-101 format: repeated { STRING(A) + UT(0x1F) + CODE(b12 = 2 bytes) }
    fn parse_code_field(data: &[u8], mapping: &mut CodeMapping) -> Result<()> {
        let mut offset = 0;

        while offset < data.len() {
            // Read string until unit terminator (0x1F) or field terminator (0x1E)
            let (code_str, consumed) = read_string(&data[offset..])?;
            offset += consumed;

            if code_str.is_empty() {
                continue;
            }

            // After string+UT, read 2-byte code number (little-endian)
            if offset + 2 > data.len() {
                break;
            }

            let code_num = u16::from_le_bytes([data[offset], data[offset + 1]]);
            offset += 2;

            tracing::trace!("Code mapping: {} -> {}", code_num, code_str);
            mapping.insert(code_num, code_str);
        }
        Ok(())
    }

    /// Process point record
    fn process_point(&mut self, dr: &DR) -> Result<()> {
        let prid_field = dr
            .find_field(tags::PRID)
            .ok_or_else(|| S100Error::MissingField("PRID".into()))?;

        let data = prid_field.data_trimmed();
        if data.len() < 5 {
            return Err(S100Error::InvalidFieldData("PRID too short".into()));
        }

        let rcnm = data[0];
        let rcid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let id = RecordId::new(rcnm, rcid);

        let coords: Vec<_> = dr
            .fields
            .iter()
            .filter(|f| matches!(f.tag.as_str(), "C2IT" | "C3IT"))
            .collect();
        if coords.len() != 1 {
            return Err(S100Error::InvalidFieldData(
                "Point needs one coordinate tuple".into(),
            ));
        }
        let f = coords[0];
        let data = f.data_trimmed();
        let three = f.tag == "C3IT";
        if data.len() != if three { 13 } else { 8 } {
            return Err(S100Error::InvalidFieldData(
                "Invalid point tuple length".into(),
            ));
        }
        let decoded = decode_coordinate_list(
            &data[usize::from(three)..],
            if three { 3 } else { 2 },
            [self.coord_factor, self.coord_factor_y, self.coord_factor_z],
            [
                self.coord_origin_x,
                self.coord_origin_y,
                self.coord_origin_z,
            ],
        )?;
        let position = decoded[0];

        let point = PointRecord {
            id,
            position,
            update_instruction: 1,
        };

        self.points.insert(id.key(), point);
        Ok(())
    }

    /// Process multi-point record (for soundings)
    /// Reference: S-100 standard/GISLibrary/R_MultiPointRecord.cpp
    fn process_multi_point(&mut self, dr: &DR) -> Result<()> {
        let mrid_field = dr
            .find_field(tags::MRID)
            .ok_or_else(|| S100Error::MissingField("MRID".into()))?;

        let data = mrid_field.data_trimmed();
        if data.len() < 5 {
            return Err(S100Error::InvalidFieldData("MRID too short".into()));
        }

        let rcnm = data[0];
        let rcid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let id = RecordId::new(rcnm, rcid);

        let mut positions = Vec::new();
        let mut vcid = None;
        for f in dr.find_fields(tags::C3IL) {
            let data = f.data_trimmed();
            if data.is_empty() {
                return Err(S100Error::InvalidFieldData("C3IL missing VCID".into()));
            }
            if vcid.is_some_and(|v| v != data[0]) {
                return Err(S100Error::InvalidFieldData(
                    "Mixed multi-point vertical CRS".into(),
                ));
            }
            vcid = Some(data[0]);
            positions.extend(decode_coordinate_list(
                &data[1..],
                3,
                [self.coord_factor, self.coord_factor_y, self.coord_factor_z],
                [
                    self.coord_origin_x,
                    self.coord_origin_y,
                    self.coord_origin_z,
                ],
            )?);
        }
        if vcid.is_none() {
            return Err(S100Error::MissingField("C3IL".into()));
        }

        let multi_point = MultiPointRecord {
            id,
            positions,
            update_instruction: 1,
        };

        self.multi_points.insert(id.key(), multi_point);
        Ok(())
    }

    /// Process curve record
    fn process_curve(&mut self, dr: &DR) -> Result<()> {
        let crid_field = dr
            .find_field(tags::CRID)
            .ok_or_else(|| S100Error::MissingField("CRID".into()))?;

        let data = crid_field.data_trimmed();
        if data.len() < 5 {
            return Err(S100Error::InvalidFieldData("CRID too short".into()));
        }

        let rcnm = data[0];
        let rcid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let id = RecordId::new(rcnm, rcid);

        // S-101 2.0.0 B-5.1.24: SEGH INTP is exclusively 4 (loxodromic).
        // Older records without an explicit header retain S-100's loxodromic
        // default; never silently reinterpret a supplied non-S101 header.
        for header in dr.find_fields("SEGH") {
            validate_s101_segment_header(header.data_trimmed())?;
        }

        // Parse start/end points from PTAS
        let mut start_point = None;
        let mut end_point = None;

        let ptas = dr.find_fields(tags::PTAS);
        if ptas.len() > 1 {
            return Err(S100Error::InvalidFieldData("Duplicate PTAS".into()));
        }
        if let Some(ptas_field) = ptas.first() {
            let ptas_data = ptas_field.data_trimmed();
            if ptas_data.len() % 6 != 0
                || ptas_data
                    .chunks_exact(6)
                    .any(|t| t[0] != 110 || !matches!(t[5], 1 | 2 | 3))
            {
                return Err(S100Error::InvalidFieldData("Invalid PTAS tuple".into()));
            }
            // Parse PTAS (point associations)
            // Format: RRNM(1) + RRID(4) + TOPI(1) repeated
            let mut offset = 0;
            while offset + 6 <= ptas_data.len() {
                let topi = ptas_data[offset + 5];
                let pt_rcnm = ptas_data[offset];
                let pt_rcid = u32::from_le_bytes([
                    ptas_data[offset + 1],
                    ptas_data[offset + 2],
                    ptas_data[offset + 3],
                    ptas_data[offset + 4],
                ]);

                let pt_id = RecordId::new(pt_rcnm, pt_rcid);
                if topi == 1 || topi == 3 {
                    start_point = Some(pt_id);
                }
                if topi == 2 || topi == 3 {
                    end_point = Some(pt_id);
                }
                offset += 6;
            }
        }

        // SEGH defines a segment; repeated coordinate fields form one ordered
        // stream within that segment. Keep original field order across segments.
        let mut segments = Vec::new();
        let mut current: Option<CurveSegment> = None;
        let mut encoding: Option<(String, Option<u8>)> = None;
        for f in &dr.fields {
            if f.tag == tags::SEGH {
                if let Some(s) = current.take() {
                    segments.push(s);
                }
                current = Some(CurveSegment {
                    segment_type: SegmentType::Line,
                    positions: Vec::new(),
                });
                encoding = None;
            } else if matches!(f.tag.as_str(), "C2IL" | "C3IL") {
                let three = f.tag == "C3IL";
                let data = f.data_trimmed();
                if three && data.is_empty() {
                    return Err(S100Error::InvalidFieldData("C3IL missing VCID".into()));
                }
                let key = (f.tag.clone(), if three { Some(data[0]) } else { None });
                if encoding.as_ref().is_some_and(|v| v != &key) {
                    return Err(S100Error::InvalidFieldData(
                        "Mixed segment coordinate encoding/vertical CRS".into(),
                    ));
                }
                encoding = Some(key);
                let decoded = decode_coordinate_list(
                    &data[usize::from(three)..],
                    if three { 3 } else { 2 },
                    [self.coord_factor, self.coord_factor_y, self.coord_factor_z],
                    [
                        self.coord_origin_x,
                        self.coord_origin_y,
                        self.coord_origin_z,
                    ],
                )?;
                current
                    .get_or_insert_with(|| CurveSegment {
                        segment_type: SegmentType::Line,
                        positions: Vec::new(),
                    })
                    .positions
                    .extend(decoded);
            }
        }
        if let Some(s) = current {
            segments.push(s);
        }

        let curve = CurveRecord {
            id,
            segments,
            start_point,
            end_point,
            update_instruction: 1,
        };

        self.curves.insert(id.key(), curve);
        Ok(())
    }

    /// Parse coordinate array from field data
    fn parse_coordinates(&self, data: &[u8]) -> Result<Vec<Coordinate>> {
        let data = data.strip_suffix(&[FIELD_TERMINATOR]).unwrap_or(data);
        decode_coordinate_list(
            data,
            2,
            [self.coord_factor, self.coord_factor_y, self.coord_factor_z],
            [
                self.coord_origin_x,
                self.coord_origin_y,
                self.coord_origin_z,
            ],
        )
    }

    /// Process composite curve record
    fn process_composite_curve(&mut self, dr: &DR) -> Result<()> {
        let ccid_field = dr
            .find_field(tags::CCID)
            .ok_or_else(|| S100Error::MissingField("CCID".into()))?;

        let data = ccid_field.data_trimmed();
        if data.len() < 5 {
            return Err(S100Error::InvalidFieldData("CCID too short".into()));
        }

        let rcnm = data[0];
        let rcid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let id = RecordId::new(rcnm, rcid);

        // Parse curve components from CUCO
        let mut curves = Vec::new();

        for cuco_field in dr.find_fields(tags::CUCO) {
            let cuco_data = cuco_field.data_trimmed();
            if cuco_data.len() % 6 != 0 {
                return Err(S100Error::InvalidFieldData(
                    "Truncated curve component".into(),
                ));
            }
            let mut offset = 0;

            // CUCO format (6 bytes per entry):
            // RCNM (1) - Record name (type: 120=Curve, 125=CompositeCurve)
            // RCID (4) - Record identifier (little-endian)
            // ORNT (1) - Orientation (1=Forward, 2=Reverse)
            while offset + 6 <= cuco_data.len() {
                let curve_rcnm = cuco_data[offset];
                let curve_rcid = u32::from_le_bytes([
                    cuco_data[offset + 1],
                    cuco_data[offset + 2],
                    cuco_data[offset + 3],
                    cuco_data[offset + 4],
                ]);
                let ornt = cuco_data[offset + 5]; // 1=Forward, 2=Reverse

                curves.push(OrientedCurve {
                    curve_id: RecordId::new(curve_rcnm, curve_rcid),
                    orientation: ornt == 1,
                });

                offset += 6;
            }
        }

        let composite = CompositeCurveRecord {
            id,
            curves,
            update_instruction: 1,
        };

        self.composite_curves.insert(id.key(), composite);
        Ok(())
    }

    /// Process surface record
    fn process_surface(&mut self, dr: &DR) -> Result<()> {
        let srid_field = dr
            .find_field(tags::SRID)
            .ok_or_else(|| S100Error::MissingField("SRID".into()))?;

        let data = srid_field.data_trimmed();
        if data.len() < 5 {
            return Err(S100Error::InvalidFieldData("SRID too short".into()));
        }

        let rcnm = data[0];
        let rcid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let id = RecordId::new(rcnm, rcid);

        // Parse ring associations from RIAS
        let mut exterior_ring = Vec::new();
        let mut interior_rings = Vec::new();

        for rias_field in dr.find_fields(tags::RIAS) {
            let rias_data = rias_field.data_trimmed();
            if rias_data.len() % 8 != 0 {
                return Err(S100Error::InvalidFieldData(
                    "Truncated ring association".into(),
                ));
            }
            let mut offset = 0;

            // RIAS format (8 bytes per entry):
            // RCNM (1) - Record name (120=Curve, 125=CompositeCurve)
            // RCID (4) - Record ID
            // ORNT (1) - Orientation (1=Forward, 2=Reverse)
            // USAG (1) - Usage (1=Exterior, 2=Interior)
            // RAUI (1) - Ring association update instruction
            while offset + 8 <= rias_data.len() {
                let curve_rcnm = rias_data[offset];
                let curve_rcid = u32::from_le_bytes([
                    rias_data[offset + 1],
                    rias_data[offset + 2],
                    rias_data[offset + 3],
                    rias_data[offset + 4],
                ]);
                let ornt = rias_data[offset + 5]; // 1=Forward, 2=Reverse
                let usag = rias_data[offset + 6]; // 1=Exterior, 2=Interior
                let _raui = rias_data[offset + 7];

                let oriented_curve = OrientedCurve {
                    curve_id: RecordId::new(curve_rcnm, curve_rcid),
                    orientation: ornt == 1, // 1=Forward, 2=Reverse
                };

                if usag == 1 {
                    // Exterior ring
                    exterior_ring.push(oriented_curve);
                } else if usag == 2 {
                    // Interior ring (hole)
                    if interior_rings.is_empty() {
                        interior_rings.push(Vec::new());
                    }
                    interior_rings.last_mut().unwrap().push(oriented_curve);
                }

                offset += 8;
            }
        }

        let surface = SurfaceRecord {
            id,
            exterior_ring,
            interior_rings,
            update_instruction: 1,
        };

        self.surfaces.insert(id.key(), surface);
        Ok(())
    }

    /// Process feature record
    fn process_feature(&mut self, dr: &DR) -> Result<()> {
        let frid_field = dr
            .find_field(tags::FRID)
            .ok_or_else(|| S100Error::MissingField("FRID".into()))?;

        let data = frid_field.data_trimmed();
        let (rcid, nftc, rver, ruin) = parse_typed_identifier(data, 100)?;

        let frid = FRID {
            rcid,
            nftc,
            rver,
            ruin,
        };

        // Debug first few features
        static LOGGED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
            tracing::trace!(
                "FRID: rcid={}, nftc={} (bytes: {:02X} {:02X})",
                rcid,
                nftc,
                data[4],
                data[5]
            );
        }

        // Parse FOID
        let foid = dr
            .find_field(tags::FOID)
            .map(|f| parse_feature_object_identifier(&f.data))
            .transpose()?;

        // Parse attributes
        let mut attributes = Vec::new();
        for attr_field in dr.find_fields(tags::ATTR) {
            self.parse_attributes(&attr_field.data, &mut attributes)?;
        }

        // Parse spatial associations
        let mut spatial_associations = Vec::new();
        for spas_field in dr.find_fields(tags::SPAS) {
            self.parse_spatial_associations(&spas_field.data, &mut spatial_associations)?;
        }

        // Parse information associations
        let mut information_associations = Vec::new();
        for inas_field in dr.find_fields(tags::INAS) {
            self.parse_info_associations(&inas_field.data, &mut information_associations)?;
        }

        // Parse feature associations
        let mut feature_associations = Vec::new();
        for fasc_field in dr.find_fields(tags::FASC) {
            self.parse_feature_associations(&fasc_field.data, &mut feature_associations)?;
        }

        // Parse MASK field (S-100 4.8.3: mask/show indicators for spatial associations)
        let mut masks = Vec::new();
        for mask_field in dr.find_fields(tags::MASK) {
            self.parse_mask_records(&mask_field.data, &mut masks)?;
        }

        // Determine primitive type from spatial associations
        let primitive_type = self.determine_primitive_type(&spatial_associations);

        let feature = FeatureRecord {
            frid,
            foid,
            attributes,
            spatial_associations,
            information_associations,
            feature_associations,
            masks,
            feature_code: None,
            primitive_type,
        };

        let id = RecordId::new(100, rcid);
        self.features.insert(id.key(), feature);
        Ok(())
    }

    /// Parse attributes from ATTR field
    fn parse_attributes(&self, data: &[u8], attrs: &mut Vec<Attribute>) -> Result<()> {
        parse_attribute_field(data, attrs)
    }

    /// Parse spatial associations from SPAS field
    fn parse_spatial_associations(
        &self,
        data: &[u8],
        assocs: &mut Vec<SpatialAssociation>,
    ) -> Result<()> {
        assocs.extend(parse_spas(data)?);
        Ok(())
    }

    /// Parse current Part 10a MASK tuples, including MUIN (7 bytes).
    fn parse_mask_records(&self, data: &[u8], masks: &mut Vec<MaskRecord>) -> Result<()> {
        masks.extend(parse_masks(data)?);
        Ok(())
    }

    /// Parse information associations from INAS field
    fn parse_info_associations(
        &self,
        data: &[u8],
        assocs: &mut Vec<InformationAssociation>,
    ) -> Result<()> {
        let data = data.strip_suffix(&[FIELD_TERMINATOR]).unwrap_or(data);
        if data.len() < 10 {
            return Err(S100Error::InvalidFieldData(
                "Association header too short".into(),
            ));
        }
        let mut attributes = Vec::new();
        self.parse_attributes(&data[10..], &mut attributes)?;
        assocs.push(InformationAssociation {
            niac: u16::from_le_bytes(data[5..7].try_into().unwrap()),
            narc: u16::from_le_bytes(data[7..9].try_into().unwrap()),
            info_id: RecordId::new(data[0], u32::from_le_bytes(data[1..5].try_into().unwrap())),
            update_instruction: data[9],
            attributes,
        });
        Ok(())
    }

    /// Parse feature associations from FASC field
    fn parse_feature_associations(
        &self,
        data: &[u8],
        assocs: &mut Vec<FeatureAssociation>,
    ) -> Result<()> {
        let data = data.strip_suffix(&[FIELD_TERMINATOR]).unwrap_or(data);
        if data.len() < 10 {
            return Err(S100Error::InvalidFieldData(
                "Association header too short".into(),
            ));
        }
        let mut attributes = Vec::new();
        self.parse_attributes(&data[10..], &mut attributes)?;
        assocs.push(FeatureAssociation {
            nfac: u16::from_le_bytes(data[5..7].try_into().unwrap()),
            narc: u16::from_le_bytes(data[7..9].try_into().unwrap()),
            feature_id: RecordId::new(data[0], u32::from_le_bytes(data[1..5].try_into().unwrap())),
            update_instruction: data[9],
            attributes,
        });
        Ok(())
    }

    /// Determine primitive type from spatial associations
    fn determine_primitive_type(&self, assocs: &[SpatialAssociation]) -> SpatialPrimitiveType {
        if assocs.is_empty() {
            return SpatialPrimitiveType::NoGeometry;
        }

        // Check first spatial association's record name
        let rcnm = assocs[0].spatial_id.rcnm;

        match rcnm {
            110 => SpatialPrimitiveType::Point,
            115 => SpatialPrimitiveType::MultiPoint,
            120 => SpatialPrimitiveType::Curve,
            125 => SpatialPrimitiveType::CompositeCurve,
            130 => SpatialPrimitiveType::Surface,
            _ => SpatialPrimitiveType::NoGeometry,
        }
    }

    /// Process information record
    fn process_information(&mut self, dr: &DR) -> Result<()> {
        let irid_field = dr
            .find_field(tags::IRID)
            .ok_or_else(|| S100Error::MissingField("IRID".into()))?;

        let data = irid_field.data_trimmed();
        let (rcid, nitc, rver, ruin) = parse_typed_identifier(data, 150)?;

        let irid = IRID {
            rcid,
            nitc,
            rver,
            ruin,
        };

        // Parse attributes
        let mut attributes = Vec::new();
        for attr_field in dr.find_fields(tags::ATTR) {
            self.parse_attributes(&attr_field.data, &mut attributes)?;
        }

        // Parse information associations
        let mut information_associations = Vec::new();
        for inas_field in dr.find_fields(tags::INAS) {
            self.parse_info_associations(&inas_field.data, &mut information_associations)?;
        }

        let info = InformationRecord {
            irid,
            attributes,
            information_associations,
            info_code: None,
        };

        let id = RecordId::new(150, rcid);
        self.information.insert(id.key(), info);
        Ok(())
    }

    /// Apply code mappings to all records
    fn apply_code_mappings(&mut self) {
        // Apply to features
        let mut unmapped_nftc = std::collections::HashSet::new();
        for feature in self.features.values_mut() {
            // Map feature type code
            feature.feature_code = self
                .code_mappings
                .feature_type_code(feature.frid.nftc)
                .cloned();

            if feature.feature_code.is_none() {
                unmapped_nftc.insert(feature.frid.nftc);
            }

            // Map attribute codes
            for attr in &mut feature.attributes {
                attr.code = self.code_mappings.attribute_code(attr.natc).cloned();
            }
        }

        if !unmapped_nftc.is_empty() {
            tracing::warn!("Unmapped NFTC codes: {:?} (not in FTCS)", unmapped_nftc);
        }

        // Apply to information records
        for info in self.information.values_mut() {
            info.info_code = self.code_mappings.info_type_code(info.irid.nitc).cloned();

            for attr in &mut info.attributes {
                attr.code = self.code_mappings.attribute_code(attr.natc).cloned();
            }
        }

        // Attributes carried by associations use the same ATCS namespace.
        for feature in self.features.values_mut() {
            for association in &mut feature.information_associations {
                for a in &mut association.attributes {
                    a.code = self.code_mappings.attribute_code(a.natc).cloned();
                }
            }
            for association in &mut feature.feature_associations {
                for a in &mut association.attributes {
                    a.code = self.code_mappings.attribute_code(a.natc).cloned();
                }
            }
        }
        for information in self.information.values_mut() {
            for association in &mut information.information_associations {
                for a in &mut association.attributes {
                    a.code = self.code_mappings.attribute_code(a.natc).cloned();
                }
            }
        }
        for associations in self.spatial_information_associations.values_mut() {
            for association in associations {
                for a in &mut association.attributes {
                    a.code = self.code_mappings.attribute_code(a.natc).cloned();
                }
            }
        }
        self.code_mappings.log_summary();
    }

    /// Separate interior rings by curve connectivity (S-100 10a-7.2.6).
    ///
    /// During RIAS parsing, all USAG=2 (interior) curves are collected into a
    /// single Vec because the RIAS field doesn't explicitly mark ring boundaries.
    /// Multiple disjoint holes end up merged. This post-processing step uses
    /// curve start/end coordinates to split them into separate closed rings.
    fn separate_interior_rings(&mut self) {
        /// Get the start and end coordinates of an oriented curve, respecting orientation.
        fn curve_endpoints(
            oc: &OrientedCurve,
            curves: &std::collections::HashMap<i64, CurveRecord>,
            composites: &std::collections::HashMap<i64, CompositeCurveRecord>,
            points: &std::collections::HashMap<i64, PointRecord>,
        ) -> Option<(Coordinate, Coordinate)> {
            let endpoint = |start: bool| -> Option<Coordinate> {
                let mut current = oc.clone();
                // No recursion or coordinate-vector copy; cyclic input fails
                // within a bounded number of record visits.
                for _ in 0..=composites.len() {
                    let original_start = start == current.orientation;
                    if let Some(curve) = curves.get(&current.curve_id.key()) {
                        let boundary = if original_start {
                            curve.start_point
                        } else {
                            curve.end_point
                        };
                        if let Some(id) = boundary {
                            return points.get(&id.key()).map(|p| p.position);
                        }
                        return if original_start {
                            curve
                                .segments
                                .iter()
                                .find_map(|s| s.positions.first().copied())
                        } else {
                            curve
                                .segments
                                .iter()
                                .rev()
                                .find_map(|s| s.positions.last().copied())
                        };
                    }
                    let composite = composites.get(&current.curve_id.key())?;
                    let child = if original_start {
                        composite.curves.first()?
                    } else {
                        composite.curves.last()?
                    };
                    current = OrientedCurve {
                        curve_id: child.curve_id,
                        orientation: current.orientation == child.orientation,
                    };
                }
                None
            };
            Some((endpoint(true)?, endpoint(false)?))
        }

        fn coords_close(a: &Coordinate, b: &Coordinate) -> bool {
            (a.x - b.x).abs() < 1e-5 && (a.y - b.y).abs() < 1e-5
        }

        let curves_ref = &self.curves;
        let composites_ref = &self.composite_curves;
        let points_ref = &self.points;

        for surface in self.surfaces.values_mut() {
            if surface.interior_rings.len() != 1 {
                continue;
            }
            let all_curves = &surface.interior_rings[0];
            if all_curves.len() <= 1 {
                continue;
            }

            // Graph-based ring separation: handles arbitrary curve ordering
            // per S-100 10a-7.2.6 "The order of ring associations is arbitrary"

            // 1. Compute endpoints for all curves
            let endpoints: Vec<Option<(Coordinate, Coordinate)>> = all_curves
                .iter()
                .map(|oc| curve_endpoints(oc, curves_ref, composites_ref, points_ref))
                .collect();

            // 2. Track which curves are used
            let mut used = vec![false; all_curves.len()];
            let mut separated: Vec<Vec<OrientedCurve>> = Vec::new();

            // 3. Find self-closing curves first (single-curve rings)
            for (i, ep) in endpoints.iter().enumerate() {
                if let Some((start, end)) = ep {
                    if coords_close(start, end) {
                        separated.push(vec![all_curves[i].clone()]);
                        used[i] = true;
                    }
                }
            }

            // 4. Build multi-curve rings by greedy endpoint matching
            loop {
                // Find first unused curve to start a new ring
                let start_idx = used.iter().position(|&u| !u);
                let start_idx = match start_idx {
                    Some(i) if endpoints[i].is_some() => i,
                    _ => break,
                };

                let mut ring = vec![all_curves[start_idx].clone()];
                used[start_idx] = true;
                let ring_start = endpoints[start_idx].unwrap().0;
                let mut ring_end = endpoints[start_idx].unwrap().1;

                // Greedily find curves that connect to the ring's end
                let mut found = true;
                while found {
                    found = false;
                    // Check if ring is closed
                    if ring.len() > 1 && coords_close(&ring_end, &ring_start) {
                        break;
                    }
                    // Search all unused curves for one that connects
                    for (i, ep) in endpoints.iter().enumerate() {
                        if used[i] {
                            continue;
                        }
                        if let Some((start, end)) = ep {
                            if coords_close(&ring_end, start) {
                                ring.push(all_curves[i].clone());
                                used[i] = true;
                                ring_end = *end;
                                found = true;
                                break; // restart search from new end
                            }
                        }
                    }
                }

                // Only keep closed rings
                if coords_close(&ring_end, &ring_start) {
                    separated.push(ring);
                } else {
                    tracing::trace!(
                        "Surface {}: discarding {} unclosed interior curves",
                        surface.id.key(),
                        ring.len()
                    );
                }
            }

            if !separated.is_empty() && separated.len() != surface.interior_rings.len() {
                tracing::debug!(
                    "Surface {}: separated {} interior curves into {} rings",
                    surface.id.key(),
                    all_curves.len(),
                    separated.len()
                );
                surface.interior_rings = separated;
            }
        }
    }

    /// Shrink all internal Vecs to their actual size
    /// Called after loading to release unused capacity (~10-15% memory savings)
    fn shrink_to_fit(&mut self) {
        // Shrink feature record Vecs
        for feature in self.features.values_mut() {
            feature.attributes.shrink_to_fit();
            feature.spatial_associations.shrink_to_fit();
            feature.information_associations.shrink_to_fit();
            feature.feature_associations.shrink_to_fit();
            feature.masks.shrink_to_fit();
        }

        // Shrink information record Vecs
        for info in self.information.values_mut() {
            info.attributes.shrink_to_fit();
            info.information_associations.shrink_to_fit();
        }

        // Shrink curve segments
        for curve in self.curves.values_mut() {
            curve.segments.shrink_to_fit();
            for segment in &mut curve.segments {
                segment.positions.shrink_to_fit();
            }
        }

        // Shrink multi-point positions
        for mp in self.multi_points.values_mut() {
            mp.positions.shrink_to_fit();
        }

        // Shrink composite curves
        for cc in self.composite_curves.values_mut() {
            cc.curves.shrink_to_fit();
        }

        // Shrink surfaces
        for surface in self.surfaces.values_mut() {
            surface.exterior_ring.shrink_to_fit();
            surface.interior_rings.shrink_to_fit();
            for ring in &mut surface.interior_rings {
                ring.shrink_to_fit();
            }
        }
    }

    /// Get cell statistics
    pub fn statistics(&self) -> CellStatistics {
        CellStatistics {
            features: self.features.len(),
            information: self.information.len(),
            points: self.points.len(),
            multi_points: self.multi_points.len(),
            curves: self.curves.len(),
            composite_curves: self.composite_curves.len(),
            surfaces: self.surfaces.len(),
        }
    }

    /// Normalize feature codes to match FC standard codes.
    ///
    /// Some data files use non-standard feature codes (e.g., "BuoyCardinal" instead of "CardinalBuoy").
    /// This method normalizes codes by checking against the FC's feature type codes.
    ///
    /// This follows the principle of using FC dynamically (not hardcoding).
    pub fn normalize_feature_codes(&mut self, fc_feature_codes: &[String]) {
        use std::collections::HashMap;

        // Pre-compute lowercase versions (one allocation per FC code, done once)
        let fc_lowercase: Vec<String> = fc_feature_codes.iter().map(|c| c.to_lowercase()).collect();

        // Build a lookup map: lowercase -> original FC code (borrowed)
        let fc_codes_lower: HashMap<&str, &str> = fc_lowercase
            .iter()
            .zip(fc_feature_codes.iter())
            .map(|(lower, orig)| (lower.as_str(), orig.as_str()))
            .collect();

        // Build a set of FC codes as &str for quick exact match lookup
        let fc_codes_set: std::collections::HashSet<&str> =
            fc_feature_codes.iter().map(|s| s.as_str()).collect();

        // Reusable buffer for lowercase conversion to avoid repeated allocations
        let mut buf = String::with_capacity(64);

        // Helper macro-like closure for lowercase conversion
        macro_rules! to_lower {
            ($s:expr) => {{
                buf.clear();
                buf.extend($s.chars().flat_map(|c| c.to_lowercase()));
                buf.as_str()
            }};
        }

        // Try to find a matching FC code for a given data code
        let mut find_fc_code = |data_code: &str| -> Option<String> {
            // First, check exact match (no allocation needed)
            if fc_codes_set.contains(data_code) {
                return Some(data_code.to_string());
            }

            // Try case-insensitive match
            if let Some(&fc_code) = fc_codes_lower.get(to_lower!(data_code)) {
                return Some(fc_code.to_string());
            }

            // Try prefix/suffix swap heuristic:
            // "BuoyCardinal" -> "CardinalBuoy", "BeaconLateral" -> "LateralBeacon"
            let prefixes = ["Buoy", "Beacon", "Restricted"];
            for prefix in prefixes {
                if let Some(suffix) = data_code.strip_prefix(prefix) {
                    // Build swapped lowercase in buffer
                    buf.clear();
                    buf.extend(suffix.chars().flat_map(|c| c.to_lowercase()));
                    buf.extend(prefix.chars().flat_map(|c| c.to_lowercase()));
                    if let Some(&fc_code) = fc_codes_lower.get(buf.as_str()) {
                        return Some(fc_code.to_string());
                    }
                }
            }

            // Try suffix-to-prefix swap:
            // "RestrictedAreaNavigational" -> "RestrictedArea" (drop suffix)
            let suffixes = ["Navigational", "Regulatory", "WarpingFacility"];
            for suffix in suffixes {
                if let Some(base) = data_code.strip_suffix(suffix) {
                    if let Some(&fc_code) = fc_codes_lower.get(to_lower!(base)) {
                        return Some(fc_code.to_string());
                    }
                }
            }

            // Special cases for compound names
            if data_code == "MooringWarpingFacility" {
                if let Some(&fc_code) = fc_codes_lower.get("mooringarea") {
                    return Some(fc_code.to_string());
                }
            }

            None
        };

        // Normalize feature codes
        let mut normalized_count = 0;
        for feature in self.features.values_mut() {
            if let Some(ref data_code) = feature.feature_code {
                if !fc_codes_set.contains(data_code.as_str()) {
                    if let Some(fc_code) = find_fc_code(data_code) {
                        tracing::debug!(
                            "Normalized feature code: {} -> {} (feature ID: {})",
                            data_code,
                            fc_code,
                            feature.frid.rcid
                        );
                        feature.feature_code = Some(fc_code);
                        normalized_count += 1;
                    }
                }
            }
        }

        if normalized_count > 0 {
            tracing::info!(
                "Normalized {} feature codes to match FC standard codes",
                normalized_count
            );
        }

        // Also normalize FTCS mappings in code_mappings
        let mut ftcs_changes = Vec::new();
        for (num, code) in &self.code_mappings.feature_types.num_to_str {
            if !fc_codes_set.contains(code.as_str()) {
                if let Some(fc_code) = find_fc_code(code) {
                    ftcs_changes.push((*num, fc_code));
                }
            }
        }

        for (num, fc_code) in ftcs_changes {
            self.code_mappings
                .feature_types
                .num_to_str
                .insert(num, fc_code);
        }
    }
}

/// Cell statistics summary
#[derive(Debug, Clone)]
pub struct CellStatistics {
    pub features: usize,
    pub information: usize,
    pub points: usize,
    pub multi_points: usize,
    pub curves: usize,
    pub composite_curves: usize,
    pub surfaces: usize,
}

impl std::fmt::Display for CellStatistics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} features, {} info, {} pts, {} multi-pts, {} curves, {} surfaces",
            self.features,
            self.information,
            self.points,
            self.multi_points,
            self.curves,
            self.surfaces
        )
    }
}

/// S-101 Annex B DSID: fixed RCNM/RCID, seven terminated strings, eight-byte
/// reference date, then language, abstract and edition. This layout also
/// applies to the published UKHO 1.0 trial cells.
/// Inspect only the DDR leader and first data record, without mmap or loading
/// the dataset body. Exactly one DSID must be in that first record; this does not
/// certify uniqueness in later records or authentication. Legacy DSID layouts
/// retain the same identification parser as ordinary base loading.
/// Caps are receiver resource limits, not product-schema limits.
pub fn inspect_dataset_identification(path: &Path) -> Result<DatasetIdentification> {
    use ferrite_iso8211::Leader;
    use std::io::{Read, Seek, SeekFrom};
    const MAX_METADATA_RECORD: usize = 1024 * 1024;
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let size = metadata.len();
    if !metadata.is_file()
        || size > crate::updates::UpdateLimits::default().max_dataset_bytes as u64
    {
        return Err(S100Error::InvalidRecord(
            "Identification dataset byte/type budget exceeded".into(),
        ));
    }
    let mut bytes = [0u8; Leader::SIZE];
    file.read_exact(&mut bytes)?;
    let ddr = Leader::parse(&bytes)?;
    let start = u64::from(ddr.record_length);
    if !ddr.is_ddr()
        || start < Leader::SIZE as u64
        || start > MAX_METADATA_RECORD as u64
        || start > size
    {
        return Err(S100Error::InvalidRecord(
            "Malformed or oversized identification DDR".into(),
        ));
    }
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bytes)?;
    let leader = Leader::parse(&bytes)?;
    let length = leader.record_length as usize;
    if !leader.is_dr()
        || !(Leader::SIZE..=MAX_METADATA_RECORD).contains(&length)
        || start
            .checked_add(length as u64)
            .is_none_or(|end| end > size)
    {
        return Err(S100Error::InvalidRecord(
            "Malformed or oversized identification DR".into(),
        ));
    }
    let mut record = vec![0u8; length];
    record[..Leader::SIZE].copy_from_slice(&bytes);
    file.read_exact(&mut record[Leader::SIZE..])?;
    let dr = DR::parse(&record)?;
    let mut fields = dr.fields.iter().filter(|f| f.tag == tags::DSID);
    let dsid = fields
        .next()
        .ok_or_else(|| S100Error::MissingField("First DR DSID".into()))?;
    if fields.next().is_some() {
        return Err(S100Error::InvalidRecord(
            "Identification record needs exactly one DSID".into(),
        ));
    }
    let data = dsid.data_trimmed();
    if data.get(..5) != Some(&[10, 1, 0, 0, 0][..]) {
        return Err(S100Error::InvalidRecord(
            "Malformed identification DSID identifier".into(),
        ));
    }
    let mut position = 5usize;
    for index in 0..10 {
        if index == 7 {
            position = position
                .checked_add(8)
                .filter(|p| *p <= data.len())
                .ok_or_else(|| {
                    S100Error::InvalidRecord("Truncated identification reference date".into())
                })?;
        }
        let tail = data
            .get(position..)
            .ok_or_else(|| S100Error::InvalidRecord("Truncated identification string".into()))?;
        let end = tail
            .iter()
            .position(|b| *b == 0x1f)
            .ok_or_else(|| S100Error::InvalidRecord("Unterminated identification string".into()))?;
        std::str::from_utf8(&tail[..end])
            .map_err(|_| S100Error::InvalidRecord("Invalid identification UTF-8".into()))?;
        position += end + 1;
    }
    parse_dataset_identification(data)
}

fn hash_chain_input(
    chain: &mut sha2::Sha256,
    index: usize,
    bytes: &[u8],
    total: &mut usize,
    max: usize,
) -> Result<()> {
    use sha2::Digest;
    *total = total
        .checked_add(bytes.len())
        .filter(|n| *n <= max)
        .ok_or_else(|| {
            S100Error::InvalidRecord("Update-chain aggregate input byte budget exceeded".into())
        })?;
    chain.update((index as u64).to_le_bytes());
    chain.update((bytes.len() as u64).to_le_bytes());
    chain.update(sha2::Sha256::digest(bytes));
    Ok(())
}

fn bounded_update_parser(path: &Path, max: usize) -> Result<Iso8211Parser> {
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take((max as u64).saturating_add(1))
        .read_to_end(&mut data)?;
    if data.len() > max {
        return Err(S100Error::InvalidRecord(
            "Update-chain input byte budget exceeded".into(),
        ));
    }
    Ok(Iso8211Parser::from_bytes(data))
}
fn parse_edition(s: &str) -> Result<(u16, u16)> {
    let parts: Vec<_> = s.split('.').collect();
    if parts.len() > 2
        || parts
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(S100Error::InvalidRecord(
            "Malformed dataset edition/update".into(),
        ));
    }
    let edition = parts[0]
        .parse::<u16>()
        .map_err(|_| S100Error::InvalidRecord("Dataset edition overflow".into()))?;
    let update = if parts.len() == 2 {
        parts[1]
            .parse::<u16>()
            .map_err(|_| S100Error::InvalidRecord("Dataset update overflow".into()))?
    } else {
        0
    };
    if edition == 0 || update > 999 {
        return Err(S100Error::InvalidRecord(
            "Invalid dataset edition/update range".into(),
        ));
    }
    Ok((edition, update))
}
fn dataset_filename(s: &str) -> Result<(String, u16)> {
    let (stem, extension) = s
        .rsplit_once('.')
        .ok_or_else(|| S100Error::InvalidRecord("Missing dataset update extension".into()))?;
    if stem.is_empty()
        || extension.len() != 3
        || !extension.bytes().all(|b| b.is_ascii_digit())
        || s.contains('/')
        || s.contains('\\')
    {
        return Err(S100Error::InvalidRecord(
            "Malformed dataset update filename".into(),
        ));
    }
    Ok((
        stem.into(),
        extension
            .parse()
            .map_err(|_| S100Error::InvalidRecord("Invalid filename update number".into()))?,
    ))
}
fn records_identification(records: &[DR]) -> Result<DatasetIdentification> {
    let fields: Vec<_> = records
        .iter()
        .flat_map(|r| r.fields.iter())
        .filter(|f| f.tag == "DSID")
        .collect();
    if fields.len() != 1 {
        return Err(S100Error::InvalidRecord(
            "Update chain needs exactly one DSID".into(),
        ));
    }
    strict_chain_dsid(fields[0].data_trimmed())?;
    parse_dataset_identification(fields[0].data_trimmed())
}
fn strict_chain_dsid(data: &[u8]) -> Result<Vec<String>> {
    if data.len() < 5 || data[..5] != [10, 1, 0, 0, 0] {
        return Err(S100Error::InvalidRecord(
            "Malformed chain DSID identifier".into(),
        ));
    }
    let mut p = 5;
    let mut values = Vec::new();
    for i in 0..10 {
        if i == 7 {
            let date = data
                .get(p..p + 8)
                .ok_or_else(|| S100Error::InvalidRecord("Truncated chain DSID date".into()))?;
            let date = std::str::from_utf8(date)
                .map_err(|_| S100Error::InvalidRecord("Invalid chain date encoding".into()))?;
            chrono::NaiveDate::parse_from_str(date, "%Y%m%d")
                .map_err(|_| S100Error::InvalidRecord("Invalid chain reference date".into()))?;
            p += 8;
        }
        let tail = data
            .get(p..)
            .ok_or_else(|| S100Error::InvalidRecord("Truncated chain DSID string".into()))?;
        let n = tail
            .iter()
            .position(|b| *b == 0x1f)
            .ok_or_else(|| S100Error::InvalidRecord("Unterminated chain DSID string".into()))?;
        let s = std::str::from_utf8(&tail[..n])
            .map_err(|_| S100Error::InvalidRecord("Invalid UTF-8 chain DSID string".into()))?;
        if matches!(i, 0 | 1 | 2 | 3 | 4 | 5 | 9)
            && (!s.is_ascii() || s.bytes().any(|b| b.is_ascii_control()))
        {
            return Err(S100Error::InvalidRecord(
                "Invalid chain identity token".into(),
            ));
        }
        values.push(s.to_owned());
        p += n + 1;
    }
    if values[0] != "S-100 Part 10a" || !matches!(values[1].as_str(), "5.2" | "5.2.0" | "5.2.1") {
        return Err(S100Error::InvalidRecord(
            "Unsupported chain encoding standard".into(),
        ));
    }
    Ok(values)
}
fn records_dsed(records: &[DR]) -> Result<String> {
    let f = records
        .iter()
        .flat_map(|r| &r.fields)
        .find(|f| f.tag == "DSID")
        .ok_or_else(|| S100Error::MissingField("DSID".into()))?;
    Ok(strict_chain_dsid(f.data_trimmed())?[9].clone())
}
fn validate_update_metadata(base: &[DR], updates: &[DR]) -> Result<()> {
    let get_dssi = |records: &[DR]| -> Result<Vec<u8>> {
        let fields: Vec<_> = records
            .iter()
            .flat_map(|r| &r.fields)
            .filter(|f| f.tag == "DSSI")
            .collect();
        if fields.len() != 1 || !s101_update_transform(fields[0].data_trimmed()) {
            return Err(S100Error::InvalidRecord(
                "Chain needs one valid S-101 DSSI".into(),
            ));
        }
        Ok(fields[0].data_trimmed()[..36].to_vec())
    };
    if get_dssi(base)? != get_dssi(updates)? {
        return Err(S100Error::InvalidRecord(
            "Chain coordinate transform differs".into(),
        ));
    }
    let mut maps = std::collections::BTreeMap::<String, CodeMapping>::new();
    for dr in updates {
        if dr.fields.first().is_some_and(|f| {
            crate::updates::UpdateRecordHeader::parse(f).is_ok_and(|h| h.is_some())
        }) {
            continue;
        }
        for f in &dr.fields {
            match f.tag.as_str() {
                "DSID" => {}
                "ATCS" | "ITCS" | "FTCS" | "IACS" | "FACS" | "ARCS" => {
                    strict_update_code_field(&f.data, maps.entry(f.tag.clone()).or_default())?;
                }
                "DSSI" | "CSID" | "CRSH" | "CSAX" | "VDAT" => {
                    let values: Vec<_> = base
                        .iter()
                        .flat_map(|r| &r.fields)
                        .filter(|b| b.tag == f.tag)
                        .collect();
                    let compatible = values.iter().any(|b| {
                        let a = b.data_trimmed();
                        let d = f.data_trimmed();
                        if f.tag == "DSSI" {
                            s101_update_transform(a)
                                && s101_update_transform(d)
                                && a[..36] == d[..36]
                        } else {
                            a == d
                        }
                    });
                    if !compatible {
                        return Err(S100Error::InvalidRecord(
                            "Update coordinate metadata differs; raw coordinate reuse rejected"
                                .into(),
                        ));
                    }
                }
                _ => {
                    return Err(S100Error::InvalidRecord(format!(
                        "Unsupported update metadata field {}",
                        f.tag
                    )))
                }
            }
        }
    }
    Ok(())
}
fn s101_update_transform(d: &[u8]) -> bool {
    d.len() == 64
        && (0..3).all(|i| f64::from_le_bytes(d[i * 8..i * 8 + 8].try_into().unwrap()) == 0.0)
        && u32::from_le_bytes(d[24..28].try_into().unwrap()) == 10_000_000
        && u32::from_le_bytes(d[28..32].try_into().unwrap()) == 10_000_000
        && u32::from_le_bytes(d[32..36].try_into().unwrap()) == 10
}
fn strict_update_code_field(data: &[u8], map: &mut CodeMapping) -> Result<()> {
    let d = data.strip_suffix(&[FIELD_TERMINATOR]).unwrap_or(data);
    let mut p = 0;
    while p < d.len() {
        let n = d[p..]
            .iter()
            .position(|b| *b == 0x1f)
            .ok_or_else(|| S100Error::InvalidRecord("Unterminated update code name".into()))?;
        let name = std::str::from_utf8(&d[p..p + n])
            .map_err(|_| S100Error::InvalidRecord("Invalid UTF-8 code name".into()))?
            .to_owned();
        p += n + 1;
        if name.is_empty() || d.len() - p < 2 {
            return Err(S100Error::InvalidRecord(
                "Truncated/empty update code mapping".into(),
            ));
        }
        let code = u16::from_le_bytes(d[p..p + 2].try_into().unwrap());
        p += 2;
        if code == 0 || map.num_to_str.contains_key(&code) || map.str_to_num.contains_key(&name) {
            return Err(S100Error::InvalidRecord(
                "Duplicate/invalid update code mapping".into(),
            ));
        }
        map.insert(code, name);
    }
    Ok(())
}
/// Input numeric codes are local to each dataset. Preserve base numbers and
/// extend each symbolic namespace deterministically, without renumbering any
/// retained feature/attribute or relying on hash-map iteration order.
fn extend_update_dictionary(base: &mut Vec<DR>, updates: &[DR]) -> Result<bool> {
    let code_tag = |tag: &str| matches!(tag, "ATCS" | "ITCS" | "FTCS" | "IACS" | "FACS" | "ARCS");
    let mut canonical = std::collections::BTreeMap::<String, CodeMapping>::new();
    let mut input = std::collections::BTreeMap::<String, CodeMapping>::new();
    for (records, maps) in [(&base[..], &mut canonical), (updates, &mut input)] {
        for f in records.iter().flat_map(|r| &r.fields) {
            if code_tag(&f.tag) {
                strict_update_code_field(&f.data, maps.entry(f.tag.clone()).or_default())?;
            }
        }
    }
    let mut changed = false;
    for (tag, incoming) in input {
        let map = canonical.entry(tag).or_default();
        let mut ordered: Vec<_> = incoming.num_to_str.into_iter().collect();
        ordered.sort_by_key(|(code, _)| *code);
        let mut available = 1u32;
        for (_, name) in ordered {
            if map.str_to_num.contains_key(&name) {
                continue;
            }
            while available <= u16::MAX as u32 && map.num_to_str.contains_key(&(available as u16)) {
                available += 1;
            }
            if available > u16::MAX as u32 {
                return Err(S100Error::InvalidRecord(
                    "Canonical code namespace exhausted".into(),
                ));
            }
            map.insert(available as u16, name);
            available += 1;
            changed = true;
        }
    }
    if !changed {
        return Ok(false);
    }
    // Build a replacement first; failure leaves the caller's registry intact.
    let mut replacement = base.clone();
    for record in &mut replacement {
        record.fields.retain(|f| !code_tag(&f.tag));
    }
    replacement.retain(|r| !r.fields.is_empty());
    let dataset = replacement
        .iter_mut()
        .find(|r| r.fields.first().is_some_and(|f| f.tag == "DSID"))
        .ok_or_else(|| {
            S100Error::InvalidRecord("Canonical dictionary has no DSID record".into())
        })?;
    for (tag, map) in canonical {
        let mut ordered: Vec<_> = map.num_to_str.into_iter().collect();
        ordered.sort_by_key(|(code, _)| *code);
        let mut data = Vec::new();
        for (code, name) in ordered {
            data.extend_from_slice(name.as_bytes());
            data.push(0x1f);
            data.extend_from_slice(&code.to_le_bytes());
        }
        data.push(FIELD_TERMINATOR);
        dataset.fields.push(RawField::new(tag, data));
    }
    *base = replacement;
    Ok(true)
}
fn remap_update_codes(base: &[DR], updates: &mut [DR]) -> Result<()> {
    let mut canonical = std::collections::BTreeMap::<String, CodeMapping>::new();
    let mut input = std::collections::BTreeMap::<String, CodeMapping>::new();
    for (records, maps) in [(base, &mut canonical), (&*updates, &mut input)] {
        for f in records.iter().flat_map(|r| &r.fields) {
            if matches!(
                f.tag.as_str(),
                "ATCS" | "ITCS" | "FTCS" | "IACS" | "FACS" | "ARCS"
            ) {
                strict_update_code_field(&f.data, maps.entry(f.tag.clone()).or_default())?;
            }
        }
    }
    let remap = |tag: &str, data: &mut [u8]| -> Result<()> {
        if data.len() < 2 {
            return Err(S100Error::InvalidRecord("Truncated numeric code".into()));
        }
        let code = u16::from_le_bytes(data[..2].try_into().unwrap());
        let name = input
            .get(tag)
            .and_then(|m| m.num_to_str.get(&code))
            .ok_or_else(|| {
                S100Error::InvalidRecord(format!("Undeclared update {tag} code {code}"))
            })?;
        let num = canonical
            .get(tag)
            .and_then(|m| m.str_to_num.get(name))
            .copied()
            .ok_or_else(|| {
                S100Error::InvalidRecord(format!("Unmapped update {tag} code {name}"))
            })?;
        data[..2].copy_from_slice(&num.to_le_bytes());
        Ok(())
    };
    let attributes = |data: &mut [u8]| -> Result<()> {
        let mut p = 0;
        while p < data.len() {
            if data.len() - p < 7 {
                return Err(S100Error::InvalidRecord(
                    "Truncated update attribute".into(),
                ));
            }
            remap("ATCS", &mut data[p..p + 2])?;
            p += 7;
            p += data[p..]
                .iter()
                .position(|b| *b == 0x1f || *b == 0)
                .map(|n| n + 1)
                .unwrap_or(data.len() - p);
        }
        Ok(())
    };
    for dr in updates {
        if !dr.fields.first().is_some_and(|f| {
            crate::updates::UpdateRecordHeader::parse(f).is_ok_and(|h| h.is_some())
        }) {
            continue;
        }
        for f in &mut dr.fields {
            let len = f.data_trimmed().len();
            let data = &mut f.data[..len];
            match f.tag.as_str() {
                "FRID" | "IRID" => {
                    crate::updates::UpdateRecordHeader::parse(&RawField::new(
                        f.tag.clone(),
                        data.to_vec(),
                    ))?;
                    let code = data.get_mut(5..7).ok_or_else(|| {
                        S100Error::InvalidRecord("Truncated update typed identifier".into())
                    })?;
                    remap(if f.tag == "FRID" { "FTCS" } else { "ITCS" }, code)?;
                }
                "ATTR" => attributes(data)?,
                "INAS" | "FASC" => {
                    if len < 10 {
                        return Err(S100Error::InvalidRecord(
                            "Truncated update association".into(),
                        ));
                    }
                    remap(
                        if f.tag == "INAS" { "IACS" } else { "FACS" },
                        &mut data[5..7],
                    )?;
                    remap("ARCS", &mut data[7..9])?;
                    attributes(&mut data[10..])?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}
fn parse_dataset_identification(data: &[u8]) -> Result<DatasetIdentification> {
    if data.len() < 5 {
        return Err(S100Error::InvalidRecord("Truncated DSID".into()));
    }
    let mut offset = 5;
    let mut strings = Vec::new();
    for _ in 0..7 {
        let (value, length) = read_string(&data[offset..])?;
        strings.push(value);
        offset += length;
    }
    if offset + 8 > data.len() {
        return Err(S100Error::InvalidRecord(
            "Truncated DSID reference date".into(),
        ));
    }
    offset += 8;
    for _ in 0..3 {
        let (value, length) = read_string(&data[offset..])?;
        strings.push(value);
        offset += length;
    }
    let (edition_number, update_number) = parse_edition(&strings[9])?;
    if strings[4] == "2" && !strings[9].contains('.') {
        return Err(S100Error::InvalidRecord(
            "Update DSED must include edition.update".into(),
        ));
    }
    Ok(DatasetIdentification {
        dataset_name: strings[5].clone(),
        dataset_title: strings[6].clone(),
        product_identifier: strings[2].clone(),
        product_edition: strings[3].clone(),
        edition_number,
        update_number,
        application_profile: strings[4].clone(),
        ..Default::default()
    })
}
#[cfg(test)]
mod update_materialization_tests {
    use super::*;
    use ferrite_iso8211::RawField;
    fn field(tag: &str, mut data: Vec<u8>) -> RawField {
        data.push(FIELD_TERMINATOR);
        RawField::new(tag.into(), data)
    }
    fn dr(fields: Vec<RawField>) -> DR {
        let base = 24 + fields.len() * 14 + 1;
        let length = base + fields.iter().map(|f| f.data.len()).sum::<usize>();
        let mut data = format!("{length:05}3D 1 00{base:05}   5504").into_bytes();
        let mut p = 0;
        for f in &fields {
            data.extend_from_slice(format!("{}{:05}{p:05}", f.tag, f.data.len()).as_bytes());
            p += f.data.len();
        }
        data.push(FIELD_TERMINATOR);
        for f in fields {
            data.extend(f.data);
        }
        DR::parse(&data).unwrap()
    }
    fn id(tag: &str, name: u8, n: u32) -> RawField {
        let mut d = vec![name];
        d.extend(n.to_le_bytes());
        d.extend(1u16.to_le_bytes());
        d.push(1);
        field(tag, d)
    }
    fn coords(values: &[i32]) -> RawField {
        let mut d = Vec::new();
        for v in values {
            d.extend(v.to_le_bytes());
            d.extend(v.to_le_bytes());
        }
        field("C2IL", d)
    }
    #[test]
    fn spatial_information_associations_are_retained_and_attribute_codes_mapped() {
        let maps = dr(vec![field("ATCS", b"text\x1f\x01\x00".to_vec())]);
        let information = dr(vec![field("IRID", vec![150, 9, 0, 0, 0, 1, 0, 1, 0, 1])]);
        let mut association = vec![150, 9, 0, 0, 0, 1, 0, 1, 0, 1];
        association.extend([1, 0, 1, 0, 0, 0, 1]);
        association.extend(b"good\x1f");
        let point = dr(vec![
            id("PRID", 110, 1),
            field("INAS", association),
            field("C2IT", vec![0; 8]),
        ]);
        let cell = S101Cell::parse_records_with_graph_check(
            Path::new("fixture.000"),
            vec![maps, point, information],
            true,
        )
        .unwrap();
        let associations = &cell.spatial_information_associations[&RecordId::new(110, 1).key()];
        assert_eq!(associations.len(), 1);
        assert_eq!(associations[0].info_id, RecordId::new(150, 9));
        assert_eq!(associations[0].attributes[0].code.as_deref(), Some("text"));
        assert_eq!(associations[0].attributes[0].atvl, "good");
    }
    #[test]
    fn complete_materialization_preserves_segment_and_component_streams() {
        let curve = dr(vec![
            id("CRID", 120, 1),
            field("SEGH", vec![4]),
            coords(&[10]),
            coords(&[20]),
            field("SEGH", vec![4]),
            coords(&[30, 40]),
        ]);
        let ptr = field("CUCO", vec![120, 1, 0, 0, 0, 1]);
        let composite = dr(vec![id("CCID", 125, 2), ptr.clone(), ptr]);
        let cell = S101Cell::parse_records_with_graph_check(
            Path::new("fixture.000"),
            vec![curve, composite],
            true,
        )
        .unwrap();
        let curve = cell.curves.get(&RecordId::new(120, 1).key()).unwrap();
        assert_eq!(curve.segments.len(), 2);
        assert_eq!(curve.segments[0].positions.len(), 2);
        assert_eq!(curve.segments[0].positions[1].x, 20.);
        assert_eq!(curve.segments[1].positions[0].x, 30.);
        assert_eq!(
            cell.composite_curves
                .get(&RecordId::new(125, 2).key())
                .unwrap()
                .curves
                .len(),
            2
        );
    }
    #[test]
    fn materialization_rejects_dangling_targets_and_cycles_without_recursing() {
        let missing = dr(vec![
            id("CCID", 125, 2),
            field("CUCO", vec![120, 99, 0, 0, 0, 1]),
        ]);
        assert!(S101Cell::parse_records_with_graph_check(
            Path::new("fixture.000"),
            vec![missing],
            true
        )
        .is_err());
        let a = dr(vec![
            id("CCID", 125, 1),
            field("CUCO", vec![125, 2, 0, 0, 0, 1]),
        ]);
        let b = dr(vec![
            id("CCID", 125, 2),
            field("CUCO", vec![125, 1, 0, 0, 0, 1]),
        ]);
        assert!(S101Cell::parse_records_with_graph_check(
            Path::new("fixture.000"),
            vec![a, b],
            true
        )
        .is_err());
    }
    #[test]
    fn point_and_ptas_malformed_payloads_do_not_produce_default_geometry() {
        assert!(S101Cell::parse_records(
            Path::new("fixture.000"),
            vec![dr(vec![id("PRID", 110, 1)])]
        )
        .is_err());
        for ptas in [vec![110, 1, 0, 0, 0], vec![110, 1, 0, 0, 0, 9]] {
            let r = dr(vec![
                id("CRID", 120, 1),
                field("PTAS", ptas),
                field("SEGH", vec![4]),
                coords(&[10, 20]),
            ]);
            assert!(S101Cell::parse_records(Path::new("fixture.000"), vec![r]).is_err());
        }
    }
    #[test]
    fn dataset_update_counter_is_not_narrowed_to_u8() {
        assert_eq!(parse_edition("3.999").unwrap(), (3, 999));
        assert_eq!(parse_edition("3").unwrap(), (3, 0));
        for s in ["3.1000", "65536.1", "0.1", "3.-1", "3.1.2", "3."] {
            assert!(parse_edition(s).is_err());
        }
        let mut d = vec![10, 1, 0, 0, 0];
        d.extend(b"S-100 Part 10a\x1f5.2\x1fINT.IHO.S-101.2.0\x1f2.0\x1f2\x1f101AA00TEST.999\x1fTitre\x1f20241016EN\x1f\x1f3.999\x1f");
        strict_chain_dsid(&d).unwrap();
        let parsed = parse_dataset_identification(&d).unwrap();
        assert_eq!(parsed.update_number, 999);
        assert!(S101Cell::parse_records(
            Path::new("fixture.999"),
            vec![dr(vec![field("DSID", d)])]
        )
        .is_err());
    }
    #[test]
    fn dictionary_extension_preserves_base_codes_and_remaps_new_names() {
        let mut base = vec![dr(vec![
            field("DSID", vec![10, 1, 0, 0, 0]),
            field("FTCS", b"OldFeature\x1f\x02\x00".to_vec()),
            field("ATCS", b"oldAttribute\x1f\x03\x00".to_vec()),
        ])];
        let mut updates = vec![
            dr(vec![
                field(
                    "FTCS",
                    b"NewFeature\x1f\x02\x00OldFeature\x1f\x01\x00".to_vec(),
                ),
                field("ATCS", b"newAttribute\x1f\x01\x00".to_vec()),
            ]),
            dr(vec![
                field("FRID", vec![100, 9, 0, 0, 0, 2, 0, 1, 0, 1]),
                field("ATTR", vec![1, 0, 1, 0, 0, 0, 1, b'7', 0x1f]),
            ]),
        ];
        extend_update_dictionary(&mut base, &updates).unwrap();
        let mut ft = CodeMapping::new();
        let mut at = CodeMapping::new();
        for f in &base[0].fields {
            if f.tag == "FTCS" {
                strict_update_code_field(&f.data, &mut ft).unwrap();
            }
            if f.tag == "ATCS" {
                strict_update_code_field(&f.data, &mut at).unwrap();
            }
        }
        assert_eq!(ft.get_numeric("OldFeature"), Some(2));
        assert_eq!(ft.get_numeric("NewFeature"), Some(1));
        assert_eq!(at.get_numeric("oldAttribute"), Some(3));
        assert_eq!(at.get_numeric("newAttribute"), Some(1));
        remap_update_codes(&base, &mut updates).unwrap();
        assert_eq!(&updates[1].fields[0].data[5..7], &[1, 0]);
        let old: Vec<_> = base
            .iter()
            .flat_map(|r| &r.fields)
            .map(|f| (f.tag.clone(), f.data.clone()))
            .collect();
        extend_update_dictionary(&mut base, &updates[..1]).unwrap();
        assert_eq!(
            old,
            base.iter()
                .flat_map(|r| &r.fields)
                .map(|f| (f.tag.clone(), f.data.clone()))
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn dictionary_extension_rejects_cross_field_duplicate_without_publication() {
        let mut base = vec![dr(vec![
            field("DSID", vec![10, 1, 0, 0, 0]),
            field("FTCS", b"Old\x1f\x01\x00".to_vec()),
        ])];
        let original = base[0].fields[1].data.clone();
        let updates = vec![dr(vec![
            field("FTCS", b"New\x1f\x01\x00".to_vec()),
            field("FTCS", b"Other\x1f\x01\x00".to_vec()),
        ])];
        assert!(extend_update_dictionary(&mut base, &updates).is_err());
        assert_eq!(base[0].fields[1].data, original);
    }
    #[test]
    fn truncated_duplicate_identifier_remap_returns_error_without_panic() {
        let metadata = dr(vec![field("FTCS", b"CautionArea\x1f\x01\x00".to_vec())]);
        let mut header = vec![100, 1, 0, 0, 0, 1, 0, 1, 0, 1];
        let mut records = vec![
            metadata.clone(),
            dr(vec![
                field("FRID", header.clone()),
                field("FRID", vec![100]),
            ]),
        ];
        assert!(remap_update_codes(&[metadata], &mut records).is_err());
        header[9] = 2;
        assert!(S101Cell::parse_records(
            Path::new("fixture.000"),
            vec![dr(vec![field("FRID", header)])]
        )
        .is_err());
    }
}
#[cfg(test)]
mod dataset_tests {
    use super::*;
    #[test]
    fn dsid_reads_reference_date_without_a_separator() {
        let mut data = vec![10, 1, 0, 0, 0];
        data.extend_from_slice(b"S-100 Part 10a\x1f5.2\x1fINT.IHO.S-101.2.0\x1f2.0\x1f1\x1f101AA00TEST.000\x1fTest title\x1f20241016EN\x1f\x1f3\x1f");
        let id = parse_dataset_identification(&data).unwrap();
        assert_eq!(id.dataset_name, "101AA00TEST.000");
        assert_eq!(id.dataset_title, "Test title");
        assert_eq!(id.product_edition, "2.0");
        assert_eq!(id.edition_number, 3);
    }
}

fn parse_typed_identifier(data: &[u8], expected_type: u8) -> Result<(u32, u16, u16, u8)> {
    if data.len() < 10 || data[0] != expected_type {
        return Err(S100Error::InvalidFieldData(
            "Invalid typed record identifier".into(),
        ));
    }
    Ok((
        u32::from_le_bytes(data[1..5].try_into().unwrap()),
        u16::from_le_bytes(data[5..7].try_into().unwrap()),
        u16::from_le_bytes(data[7..9].try_into().unwrap()),
        data[9],
    ))
}
fn parse_spas(data: &[u8]) -> Result<Vec<SpatialAssociation>> {
    let data = data.strip_suffix(&[FIELD_TERMINATOR]).unwrap_or(data);
    if data.len() % 15 != 0 {
        return Err(S100Error::InvalidFieldData(
            "SPAS length is not a multiple of 15".into(),
        ));
    }
    let nullable_scale = |v: u32| {
        if v == 0 || v == u32::MAX {
            None
        } else {
            Some(v)
        }
    };
    Ok(data
        .chunks_exact(15)
        .map(|d| SpatialAssociation {
            spatial_id: RecordId::new(d[0], u32::from_le_bytes(d[1..5].try_into().unwrap())),
            ornt: d[5] as i8,
            usag: 0,
            mask: 0,
            scale_minimum: nullable_scale(u32::from_le_bytes(d[6..10].try_into().unwrap())),
            scale_maximum: nullable_scale(u32::from_le_bytes(d[10..14].try_into().unwrap())),
            update_instruction: d[14],
        })
        .collect())
}
#[cfg(test)]
mod identifier_tests {
    use super::*;
    #[test]
    fn typed_id_includes_rcnm_and_little_endian_type() {
        assert_eq!(
            parse_typed_identifier(&[100, 1, 0, 0, 0, 2, 1, 3, 0, 1], 100).unwrap(),
            (1, 258, 3, 1)
        );
        assert!(parse_typed_identifier(&[100, 1, 0], 100).is_err());
    }
    #[test]
    fn spatial_associations_have_fifteen_byte_stride_and_scale_nulls() {
        let mut d = vec![110, 1, 0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 1];
        d.extend_from_slice(&[130, 2, 0, 0, 0, 1, 16, 39, 0, 0, 232, 3, 0, 0, 1]);
        d.push(FIELD_TERMINATOR);
        let a = parse_spas(&d).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].spatial_id.rcid, 1);
        assert_eq!(a[0].scale_minimum, None);
        assert_eq!(a[1].scale_minimum, Some(10000));
        assert_eq!(a[1].scale_maximum, Some(1000));
    }
}

/// Decode bounded binary payload; delimiter-valued bytes are ordinary integers.
fn decode_coordinate_list(
    data: &[u8],
    dimensions: usize,
    factors: [f64; 3],
    origins: [f64; 3],
) -> Result<Vec<Coordinate>> {
    let stride = dimensions * 4;
    if !(2..=3).contains(&dimensions) || data.len() % stride != 0 {
        return Err(S100Error::InvalidFieldData(
            "Invalid coordinate list length".into(),
        ));
    }
    Ok(data
        .chunks_exact(stride)
        .map(|d| {
            let y =
                i32::from_le_bytes(d[0..4].try_into().unwrap()) as f64 * factors[1] + origins[1];
            let x =
                i32::from_le_bytes(d[4..8].try_into().unwrap()) as f64 * factors[0] + origins[0];
            if dimensions == 3 {
                let z = i32::from_le_bytes(d[8..12].try_into().unwrap()) as f64 * factors[2]
                    + origins[2];
                Coordinate::new_3d(x, y, z)
            } else {
                Coordinate::new(x, y)
            }
        })
        .collect())
}
#[cfg(test)]
mod coordinate_tests {
    use super::*;
    #[test]
    fn delimiter_values_do_not_truncate_integer_coordinates() {
        let p = decode_coordinate_list(
            &[30, 0, 0, 0, 31, 0, 0, 0, 31, 0, 0, 0, 30, 0, 0, 0],
            2,
            [1.0; 3],
            [0.0; 3],
        )
        .unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].x, p[0].y), (31.0, 30.0));
        assert_eq!((p[1].x, p[1].y), (30.0, 31.0));
    }
    #[test]
    fn coordinate_axes_have_independent_factors_and_origins() {
        let p = decode_coordinate_list(
            &[20, 0, 0, 0, 10, 0, 0, 0, 30, 0, 0, 0],
            3,
            [0.1, 0.01, 0.001],
            [1.0, 2.0, 3.0],
        )
        .unwrap();
        assert_eq!(p[0].x, 2.0);
        assert_eq!(p[0].y, 2.2);
        assert!((p[0].depth().unwrap() - 3.03).abs() < 1e-12);
    }
}

/// Normalize field-local PAIX to the combined internal attribute vector.
/// Stage first so malformed input never leaves a partially appended field.
fn parse_attribute_field(data: &[u8], attrs: &mut Vec<Attribute>) -> Result<()> {
    let data = data.strip_suffix(&[FIELD_TERMINATOR]).unwrap_or(data);
    let mut offset = 0;
    let mut field = Vec::new();
    while offset < data.len() {
        if data.len() - offset < 7 {
            return Err(S100Error::InvalidFieldData("Truncated ATTR tuple".into()));
        }
        let natc = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
        let atix = u16::from_le_bytes(data[offset + 2..offset + 4].try_into().unwrap());
        let paix = u16::from_le_bytes(data[offset + 4..offset + 6].try_into().unwrap());
        offset += 7;
        let (atvl, consumed) = read_string(&data[offset..])?;
        offset += consumed;
        field.push(Attribute {
            natc,
            atix,
            paix,
            atvl,
            value: None,
            code: None,
        });
    }
    let count = field.len();
    for a in &mut field {
        if a.paix > 0 {
            if a.paix as usize > count {
                return Err(S100Error::InvalidFieldData(
                    "ATTR parent outside its field".into(),
                ));
            }
            a.paix = u16::try_from(attrs.len() + a.paix as usize).map_err(|_| {
                S100Error::InvalidFieldData("Combined ATTR parent index overflow".into())
            })?;
        }
    }
    attrs.extend(field);
    Ok(())
}
fn parse_feature_object_identifier(data: &[u8]) -> Result<FOID> {
    let data = if data.len() == 9 && data[8] == FIELD_TERMINATOR {
        &data[..8]
    } else {
        data
    };
    if data.len() != 8 {
        return Err(S100Error::InvalidFieldData(
            "FOID must contain 8 binary bytes".into(),
        ));
    }
    Ok(FOID {
        agen: u16::from_le_bytes(data[..2].try_into().unwrap()),
        fidn: u32::from_le_bytes(data[2..6].try_into().unwrap()),
        fids: u16::from_le_bytes(data[6..8].try_into().unwrap()),
    })
}
#[cfg(test)]
mod owned_source_identity_tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ferrite-owned-cell-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dataset(title: &str) -> Vec<u8> {
        let mut bytes = b"000253LE1 0000025 ! 1104\x1e".to_vec();
        let mut dsid = vec![10, 1, 0, 0, 0];
        dsid.extend_from_slice(
            b"S-100 Part 10a\x1f5.2\x1fINT.IHO.S-101.2.0\x1f2.0\x1f1\x1f101AA00TEST.000\x1f",
        );
        dsid.extend_from_slice(title.as_bytes());
        dsid.extend_from_slice(b"\x1f20241016EN\x1f\x1f3\x1f\x1e");
        // One DSID field: tag4 + length4 + position5 + directory terminator.
        let mut leader = b"000003DE1 0000038 ! 4504".to_vec();
        assert_eq!(leader.len(), 24);
        leader[..5].copy_from_slice(format!("{:05}", 38 + dsid.len()).as_bytes());
        bytes.extend(leader);
        bytes.extend_from_slice(format!("DSID{:04}00000", dsid.len()).as_bytes());
        bytes.push(FIELD_TERMINATOR);
        bytes.extend(dsid);
        bytes
    }

    #[test]
    fn overwrite_and_delete_after_capture_cannot_change_parsed_title_or_digest() {
        let temp = Temp::new();
        let path = temp.0.join("cell.000");
        let a = dataset("Captured A");
        let b = dataset("Replacement B with a different length");
        std::fs::write(&path, &a).unwrap();
        let captured = Iso8211Parser::from_file(&path).unwrap();
        std::fs::write(&path, &b).unwrap();
        let (later, later_identity) = S101Cell::load_from_with_identity(&path, &path).unwrap();
        assert_eq!(
            later.dsid.dataset_title,
            "Replacement B with a different length"
        );
        assert_eq!(
            later_identity.sha256().as_slice(),
            Sha256::digest(&b).as_slice()
        );
        std::fs::remove_file(&path).unwrap();
        let (original, original_identity) =
            S101Cell::parse_owned_with_identity(&path, captured).unwrap();
        assert_eq!(original.file_path, path);
        assert_eq!(original.dsid.dataset_title, "Captured A");
        assert_eq!(
            original_identity.sha256().as_slice(),
            Sha256::digest(&a).as_slice()
        );
        assert_ne!(original_identity, later_identity);
    }

    #[test]
    fn owned_and_legacy_load_keep_dsid_and_invalid_input_behavior() {
        let temp = Temp::new();
        let path = temp.0.join("cell.000");
        let bytes = dataset("Same parsed dataset");
        std::fs::write(&path, &bytes).unwrap();
        let mapped = S101Cell::load_from(&path, &path).unwrap();
        let (owned, _) = S101Cell::load_from_with_identity(&path, &path).unwrap();
        assert_eq!(format!("{mapped:?}"), format!("{owned:?}"));
        std::fs::write(&path, b"invalid").unwrap();
        let mapped_error = S101Cell::load_from(&path, &path).unwrap_err().to_string();
        let owned_error = S101Cell::load_from_with_identity(&path, &path)
            .unwrap_err()
            .to_string();
        assert_eq!(mapped_error, owned_error);
    }
    fn input_dataset(title: &str, profile: u8, number: u16) -> Vec<u8> {
        let mut dsid = vec![10, 1, 0, 0, 0];
        let edition = if profile == 1 {
            "3".to_owned()
        } else {
            format!("3.{number}")
        };
        dsid.extend(format!("S-100 Part 10a\x1f5.2\x1fINT.IHO.S-101.2.0\x1f2.0\x1f{profile}\x1f101AA00TEST.{number:03}\x1f{title}\x1f20241016EN\x1f\x1f{edition}\x1f").as_bytes());
        let mut dssi = vec![0; 64];
        dssi[24..28].copy_from_slice(&10_000_000u32.to_le_bytes());
        dssi[28..32].copy_from_slice(&10_000_000u32.to_le_bytes());
        dssi[32..36].copy_from_slice(&10u32.to_le_bytes());
        encode_tagged_fields(&[("DSID", dsid), ("DSSI", dssi)])
    }
    fn encode_identification_fields(fields: &[Vec<u8>]) -> Vec<u8> {
        encode_tagged_fields(
            &fields
                .iter()
                .map(|f| ("DSID", f.clone()))
                .collect::<Vec<_>>(),
        )
    }
    fn encode_tagged_fields(fields: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = b"000253LE1 0000025 ! 1104\x1e".to_vec();
        let base = 24 + 13 * fields.len() + 1;
        let length = base + fields.iter().map(|(_, f)| f.len() + 1).sum::<usize>();
        let mut leader = b"000003DE1 0000038 ! 4504".to_vec();
        leader[..5].copy_from_slice(format!("{length:05}").as_bytes());
        leader[12..17].copy_from_slice(format!("{base:05}").as_bytes());
        bytes.extend(leader);
        let mut offset = 0;
        for (tag, field) in fields {
            bytes.extend(format!("{tag}{:04}{offset:05}", field.len() + 1).as_bytes());
            offset += field.len() + 1;
        }
        bytes.push(FIELD_TERMINATOR);
        for (_, field) in fields {
            bytes.extend(field);
            bytes.push(FIELD_TERMINATOR);
        }
        bytes
    }
    #[test]
    fn metadata_inspection_is_prefix_only_and_rejects_malformed_inputs() {
        let temp = Temp::new();
        let path = temp.0.join("inspect.000");
        let good = input_dataset("Original title", 1, 0);
        std::fs::write(&path, &good).unwrap();
        assert_eq!(
            inspect_dataset_identification(&path).unwrap().dataset_title,
            "Original title"
        );
        let mut trailing = good.clone();
        trailing.extend(b"unparsed malformed record body");
        std::fs::write(&path, trailing).unwrap();
        assert_eq!(
            inspect_dataset_identification(&path).unwrap().dataset_title,
            "Original title"
        );
        for length in [0, 23, 25, good.len() - 1] {
            std::fs::write(&path, &good[..length]).unwrap();
            assert!(inspect_dataset_identification(&path).is_err());
        }
        let dr = DR::parse(&good[25..]).unwrap();
        let field = dr.fields[0].data_trimmed().to_vec();
        std::fs::write(
            &path,
            encode_identification_fields(&[field.clone(), field.clone()]),
        )
        .unwrap();
        assert!(inspect_dataset_identification(&path).is_err());
        let mut malformed = field.clone();
        malformed[0] = 99;
        std::fs::write(&path, encode_identification_fields(&[malformed])).unwrap();
        assert!(inspect_dataset_identification(&path).is_err());
        let mut malformed = field;
        malformed.pop();
        std::fs::write(&path, encode_identification_fields(&[malformed])).unwrap();
        assert!(inspect_dataset_identification(&path).is_err());
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(crate::updates::UpdateLimits::default().max_dataset_bytes as u64 + 1)
            .unwrap();
        assert!(inspect_dataset_identification(&path).is_err());
    }
    #[test]
    fn private_empty_chain_keeps_raw_identity_legacy_and_original_source() {
        let temp = Temp::new();
        let private = temp.0.join("private.000");
        let source = temp.0.join("original-does-not-exist.000");
        let mut bytes = dataset("Legacy retained title");
        // Product edition same width: ordinary base loader permits legacy1.0.
        let marker = b"INT.IHO.S-101.2.0\x1f2.0";
        let start = bytes
            .windows(marker.len())
            .position(|w| w == marker)
            .unwrap();
        bytes[start..start + marker.len()].copy_from_slice(b"INT.IHO.S-101.1.0\x1f1.0");
        std::fs::write(&private, &bytes).unwrap();
        assert_eq!(
            inspect_dataset_identification(&private)
                .unwrap()
                .product_edition,
            "1.0"
        );
        let (ordinary, a) = S101Cell::load_from_with_identity(&source, &private).unwrap();
        let (chain, b) =
            S101Cell::load_update_chain_from_with_identity(&source, &private, &[]).unwrap();
        assert_eq!(format!("{ordinary:?}"), format!("{chain:?}"));
        assert_eq!(a, b);
        assert_eq!(chain.file_path, source);
        assert_eq!(
            b.sha256().as_slice(),
            sha2::Sha256::digest(&bytes).as_slice()
        );
    }
    #[test]
    fn update_chain_identity_commits_order_and_raw_mutation() {
        use sha2::Digest;
        let temp = Temp::new();
        let base = temp.0.join("private.000");
        let a = temp.0.join("private.001");
        let b = temp.0.join("private.002");
        let source = temp.0.join("original.000");
        std::fs::write(&base, input_dataset("Base", 1, 0)).unwrap();
        std::fs::write(&a, input_dataset("First", 2, 1)).unwrap();
        std::fs::write(&b, input_dataset("Second", 2, 2)).unwrap();
        let (cell, identity) =
            S101Cell::load_update_chain_from_with_identity(&source, &base, &[a.clone(), b.clone()])
                .unwrap();
        assert_eq!(cell.file_path, source);
        assert_eq!(cell.dsid.update_number, 2);
        assert!(S101Cell::load_update_chain_from_with_identity(
            &source,
            &base,
            &[b.clone(), a.clone()]
        )
        .is_err());
        std::fs::write(&a, input_dataset("Changed raw update title", 2, 1)).unwrap();
        let (_, changed) =
            S101Cell::load_update_chain_from_with_identity(&source, &base, &[a.clone(), b.clone()])
                .unwrap();
        assert_ne!(identity, changed);
        let raw_base = sha2::Sha256::digest(std::fs::read(&base).unwrap());
        assert_ne!(identity.sha256().as_slice(), raw_base.as_slice());
        let mut x = sha2::Sha256::new();
        let mut y = sha2::Sha256::new();
        let mut tx = 0;
        let mut ty = 0;
        hash_chain_input(&mut x, 0, b"A", &mut tx, 2).unwrap();
        hash_chain_input(&mut x, 1, b"B", &mut tx, 2).unwrap();
        hash_chain_input(&mut y, 0, b"B", &mut ty, 2).unwrap();
        hash_chain_input(&mut y, 1, b"A", &mut ty, 2).unwrap();
        assert_ne!(x.finalize(), y.finalize());
        let mut limit = sha2::Sha256::new();
        let mut total = 0;
        assert!(hash_chain_input(&mut limit, 0, b"ABC", &mut total, 2).is_err());
    }
}

#[cfg(test)]
mod identity_attribute_tests {
    use super::*;
    fn tuple(code: u16, index: u16, parent: u16) -> Vec<u8> {
        let mut d = Vec::new();
        for v in [code, index, parent] {
            d.extend(v.to_le_bytes());
        }
        d.extend([1, b'x', 0x1f]);
        d
    }
    #[test]
    fn repeated_attr_fields_preserve_parent_scope_and_repeated_occurrence_indices() {
        let mut attrs = Vec::new();
        let mut data = tuple(1, 1, 0);
        data.extend(tuple(2, 1, 1));
        data.push(FIELD_TERMINATOR);
        parse_attribute_field(&data, &mut attrs).unwrap();
        parse_attribute_field(&data, &mut attrs).unwrap();
        assert_eq!(
            attrs.iter().map(|a| a.paix).collect::<Vec<_>>(),
            vec![0, 1, 0, 3]
        );
        assert!(attrs.iter().all(|a| a.atix == 1));
        let before = attrs.len();
        assert!(parse_attribute_field(&tuple(3, 1, 2), &mut attrs).is_err());
        assert_eq!(attrs.len(), before);
        assert!(parse_attribute_field(&[1, 2, 3], &mut attrs).is_err());
        assert_eq!(attrs.len(), before);
    }
    #[test]
    fn foid_binary_delimiters_are_preserved_and_short_fields_rejected() {
        let original = FOID {
            agen: 30,
            fidn: 0x1f001e,
            fids: 0x1f1e,
        };
        let mut data = Vec::new();
        data.extend(original.agen.to_le_bytes());
        data.extend(original.fidn.to_le_bytes());
        data.extend(original.fids.to_le_bytes());
        data.push(FIELD_TERMINATOR);
        assert_eq!(parse_feature_object_identifier(&data).unwrap(), original);
        assert!(parse_feature_object_identifier(&data[..7]).is_err());
        assert_eq!(original.to_string(), "30:2031646:7966");
        assert_eq!(
            parse_feature_object_identifier(&data[..8]).unwrap(),
            original
        );
    }
}

fn parse_masks(data: &[u8]) -> Result<Vec<MaskRecord>> {
    let data = data.strip_suffix(&[FIELD_TERMINATOR]).unwrap_or(data);
    if data.len() % 7 != 0 {
        return Err(S100Error::InvalidFieldData(
            "MASK length is not a multiple of 7".into(),
        ));
    }
    data.chunks_exact(7)
        .map(|d| {
            if !matches!(d[5], 1 | 2) || !matches!(d[6], 1 | 2) {
                return Err(S100Error::InvalidFieldData(
                    "Invalid MASK MIND or MUIN".into(),
                ));
            }
            Ok(MaskRecord {
                spatial_id: RecordId::new(d[0], u32::from_le_bytes(d[1..5].try_into().unwrap())),
                mask_type: d[5],
                update_instruction: d[6],
            })
        })
        .collect()
}
#[cfg(test)]
mod mask_tests {
    use super::*;
    #[test]
    fn masks_have_seven_byte_stride_and_retain_update_instruction() {
        let data = [
            120,
            30,
            0,
            0,
            0,
            2,
            1,
            125,
            31,
            0,
            0,
            0,
            1,
            2,
            FIELD_TERMINATOR,
        ];
        let m = parse_masks(&data).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].spatial_id.rcid, 30);
        assert_eq!(m[0].mask_type, 2);
        assert_eq!(m[0].update_instruction, 1);
        assert_eq!(m[1].spatial_id.rcnm, 125);
        assert_eq!(m[1].spatial_id.rcid, 31);
        assert_eq!(m[1].update_instruction, 2);
        assert!(parse_masks(&data[..6]).is_err());
        assert!(parse_masks(&[120, 1, 0, 0, 0, 3, 1]).is_err());
    }
}

fn validate_s101_segment_header(data: &[u8]) -> Result<()> {
    if data != [4] {
        return Err(S100Error::InvalidFieldData(format!(
            "S-101 SEGH must contain one loxodromic INTP=4 byte; got {data:?}"
        )));
    }
    Ok(())
}
#[cfg(test)]
mod curve_interpolation_tests {
    use super::*;
    #[test]
    fn s101_rejects_nonloxodromic_or_malformed_segment_headers() {
        assert!(validate_s101_segment_header(&[4]).is_ok());
        for bytes in [
            vec![],
            vec![0],
            vec![1],
            vec![2],
            vec![3],
            vec![5],
            vec![4, 4],
        ] {
            assert!(validate_s101_segment_header(&bytes).is_err());
        }
    }
}
