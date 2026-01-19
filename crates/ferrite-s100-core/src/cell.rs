//! S-101 Cell container
//!
//! Represents a complete S-101 ENC cell with all records.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ferrite_iso8211::{Iso8211Parser, DR, tags, read_string, UNIT_TERMINATOR, FIELD_TERMINATOR};

use crate::{
    S100Error, Result,
    RecordId, Coordinate,
    PointRecord, MultiPointRecord, CurveRecord, CurveSegment, SegmentType,
    CompositeCurveRecord, OrientedCurve, SurfaceRecord,
    FeatureRecord, FRID, FOID, Attribute, SpatialAssociation, InformationAssociation,
    FeatureAssociation, SpatialPrimitiveType,
    InformationRecord, IRID,
    DatasetCodeMappings, CodeMapping,
};

/// Dataset identification
#[derive(Debug, Clone, Default)]
pub struct DatasetIdentification {
    pub dataset_name: String,
    pub edition_number: u8,
    pub update_number: u8,
    pub update_application_date: String,
    pub issue_date: String,
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
    /// Coordinate multiplication factor for Z (1/CMFZ)
    pub coord_factor_z: f64,
    /// Coordinate origin X (DCOX)
    pub coord_origin_x: f64,
    /// Coordinate origin Y (DCOY)
    pub coord_origin_y: f64,
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
}

impl S101Cell {
    /// Load cell from file
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        tracing::info!("Loading S-101 cell: {}", path.display());

        let mut parser = Iso8211Parser::from_file(path)?;
        let (_ddr, records) = parser.read_all()?;

        let mut cell = S101Cell {
            file_path: path.to_path_buf(),
            dsid: DatasetIdentification::default(),
            code_mappings: DatasetCodeMappings::new(),
            coord_factor: 1.0,
            coord_factor_z: 0.01, // Default CMFZ=100, so factor = 1/100
            coord_origin_x: 0.0,
            coord_origin_y: 0.0,
            points: HashMap::new(),
            multi_points: HashMap::new(),
            curves: HashMap::new(),
            composite_curves: HashMap::new(),
            surfaces: HashMap::new(),
            features: HashMap::new(),
            information: HashMap::new(),
        };

        // Process records
        for dr in records {
            cell.process_record(&dr)?;
        }

        // Apply code mappings to records
        cell.apply_code_mappings();

        tracing::info!(
            "Loaded cell: {} features, {} points, {} curves, {} surfaces",
            cell.features.len(),
            cell.points.len(),
            cell.curves.len(),
            cell.surfaces.len()
        );

        Ok(cell)
    }

    /// Process a single data record
    fn process_record(&mut self, dr: &DR) -> Result<()> {
        // Determine record type by first field tag
        if let Some(first_field) = dr.fields.first() {
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
            let _data = field.data_trimmed();
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
                    data[0], data[1], data[2], data[3],
                    data[4], data[5], data[6], data[7],
                ]);
                let dcoy = f64::from_le_bytes([
                    data[8], data[9], data[10], data[11],
                    data[12], data[13], data[14], data[15],
                ]);

                self.coord_origin_x = dcox;
                self.coord_origin_y = dcoy;

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
        let prid_field = dr.find_field(tags::PRID)
            .ok_or_else(|| S100Error::MissingField("PRID".into()))?;

        let data = prid_field.data_trimmed();
        if data.len() < 5 {
            return Err(S100Error::InvalidFieldData("PRID too short".into()));
        }

        let rcnm = data[0];
        let rcid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let id = RecordId::new(rcnm, rcid);

        // Parse coordinates from C2IL or C3IL field
        let mut position = Coordinate::new(0.0, 0.0);

        // Look for C2IT (2D Integer Tuple) for points, not C2IL (which is for lists)
        if let Some(coord_field) = dr.find_field(tags::C2IT).or_else(|| dr.find_field(tags::C3IT)) {
            let coord_data = coord_field.data_trimmed();
            if coord_data.len() >= 8 {
                // S-101 stores coordinates as (YCOO, XCOO) = (latitude, longitude)
                let y = i32::from_le_bytes([coord_data[0], coord_data[1], coord_data[2], coord_data[3]]);
                let x = i32::from_le_bytes([coord_data[4], coord_data[5], coord_data[6], coord_data[7]]);
                position = Coordinate::new(
                    x as f64 * self.coord_factor + self.coord_origin_x,
                    y as f64 * self.coord_factor + self.coord_origin_y,
                );
            }
        }

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
        let mrid_field = dr.find_field(tags::MRID)
            .ok_or_else(|| S100Error::MissingField("MRID".into()))?;

        let data = mrid_field.data_trimmed();
        if data.len() < 5 {
            return Err(S100Error::InvalidFieldData("MRID too short".into()));
        }

        let rcnm = data[0];
        let rcid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let id = RecordId::new(rcnm, rcid);

        // Parse 3D coordinates from C3IL field
        // C3IL contains VCID (vertical CRS ID) followed by coordinate triplets
        let mut positions = Vec::new();

        if let Some(coord_field) = dr.find_field(tags::C3IL) {
            let coord_data = coord_field.data_trimmed();
            // First byte is VCID (Vertical CRS ID), skip it
            // Then coordinates are (YCOO, XCOO, ZCOO) triplets, each 4 bytes (b24 stored in 4)
            // Actually, based on S-100 spec, each coordinate is b24 (3 bytes), but often stored as 4-byte aligned

            // According to S-100 standard F_C3IL.cpp, VCID is first 1 byte, then triplets
            // Each triplet: YCOO(4) + XCOO(4) + ZCOO(4) = 12 bytes per point
            if coord_data.len() > 1 {
                let _vcid = coord_data[0]; // Vertical CRS ID (ignored for now)
                let triplet_data = &coord_data[1..];

                // Each coordinate triplet is 12 bytes: Y(4) + X(4) + Z(4)
                let triplet_size = 12;
                let num_points = triplet_data.len() / triplet_size;

                for i in 0..num_points {
                    let offset = i * triplet_size;
                    if offset + 12 <= triplet_data.len() {
                        let y = i32::from_le_bytes([
                            triplet_data[offset], triplet_data[offset + 1],
                            triplet_data[offset + 2], triplet_data[offset + 3],
                        ]);
                        let x = i32::from_le_bytes([
                            triplet_data[offset + 4], triplet_data[offset + 5],
                            triplet_data[offset + 6], triplet_data[offset + 7],
                        ]);
                        let z = i32::from_le_bytes([
                            triplet_data[offset + 8], triplet_data[offset + 9],
                            triplet_data[offset + 10], triplet_data[offset + 11],
                        ]);

                        let coord = Coordinate::new_3d(
                            x as f64 * self.coord_factor + self.coord_origin_x,
                            y as f64 * self.coord_factor + self.coord_origin_y,
                            z as f64 * self.coord_factor_z, // Z uses CMFZ factor, no origin offset
                        );
                        positions.push(coord);
                    }
                }
            }
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
        let crid_field = dr.find_field(tags::CRID)
            .ok_or_else(|| S100Error::MissingField("CRID".into()))?;

        let data = crid_field.data_trimmed();
        if data.len() < 5 {
            return Err(S100Error::InvalidFieldData("CRID too short".into()));
        }

        let rcnm = data[0];
        let rcid = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let id = RecordId::new(rcnm, rcid);

        // Parse start/end points from PTAS
        let mut start_point = None;
        let mut end_point = None;

        if let Some(ptas_field) = dr.find_field(tags::PTAS) {
            let ptas_data = ptas_field.data_trimmed();
            // Parse PTAS (point associations)
            // Format: TOPI(1) + RCNM(1) + RCID(4) repeated
            let mut offset = 0;
            while offset + 6 <= ptas_data.len() {
                let topi = ptas_data[offset];
                let pt_rcnm = ptas_data[offset + 1];
                let pt_rcid = u32::from_le_bytes([
                    ptas_data[offset + 2],
                    ptas_data[offset + 3],
                    ptas_data[offset + 4],
                    ptas_data[offset + 5],
                ]);

                let pt_id = RecordId::new(pt_rcnm, pt_rcid);
                if topi == 1 {
                    start_point = Some(pt_id);
                } else if topi == 2 {
                    end_point = Some(pt_id);
                }
                offset += 6;
            }
        }

        // Parse segments
        let mut segments = Vec::new();

        // Look for coordinate fields
        for coord_tag in &[tags::C2IL, tags::C3IL] {
            for field in dr.find_fields(coord_tag) {
                let positions = self.parse_coordinates(&field.data)?;
                if !positions.is_empty() {
                    segments.push(CurveSegment {
                        segment_type: SegmentType::Line,
                        positions,
                    });
                }
            }
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
        let mut coords = Vec::new();
        let mut offset = 0;

        while offset + 8 <= data.len() {
            // Check for terminators
            if data[offset] == UNIT_TERMINATOR || data[offset] == FIELD_TERMINATOR {
                break;
            }

            // S-101 stores coordinates as (YCOO, XCOO) = (latitude, longitude)
            let y_raw = i32::from_le_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]);
            let x_raw = i32::from_le_bytes([data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7]]);

            let x = x_raw as f64 * self.coord_factor + self.coord_origin_x;
            let y = y_raw as f64 * self.coord_factor + self.coord_origin_y;

            coords.push(Coordinate::new(x, y));

            offset += 8;
        }

        Ok(coords)
    }

    /// Process composite curve record
    fn process_composite_curve(&mut self, dr: &DR) -> Result<()> {
        let ccid_field = dr.find_field(tags::CCID)
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

        if let Some(cuco_field) = dr.find_field(tags::CUCO) {
            let cuco_data = cuco_field.data_trimmed();
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
        let srid_field = dr.find_field(tags::SRID)
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

        if let Some(rias_field) = dr.find_field(tags::RIAS) {
            let rias_data = rias_field.data_trimmed();
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
        let frid_field = dr.find_field(tags::FRID)
            .ok_or_else(|| S100Error::MissingField("FRID".into()))?;

        let data = frid_field.data_trimmed();
        if data.len() < 8 {
            return Err(S100Error::InvalidFieldData("FRID too short".into()));
        }

        let rcid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        // NFTC is stored as big-endian (high byte first)
        let nftc = u16::from_be_bytes([data[4], data[5]]);
        let rver = u16::from_le_bytes([data[6], data[7]]);
        let ruin = if data.len() > 8 { data[8] } else { 1 };

        let frid = FRID { rcid, nftc, rver, ruin };

        // Debug first few features
        static LOGGED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
            tracing::trace!(
                "FRID: rcid={}, nftc={} (bytes: {:02X} {:02X})",
                rcid, nftc, data[4], data[5]
            );
        }

        // Parse FOID
        let foid = dr.find_field(tags::FOID).and_then(|f| {
            let d = f.data_trimmed();
            if d.len() >= 8 {
                Some(FOID {
                    agen: u16::from_le_bytes([d[0], d[1]]),
                    fidn: u32::from_le_bytes([d[2], d[3], d[4], d[5]]),
                    fids: u16::from_le_bytes([d[6], d[7]]),
                })
            } else {
                None
            }
        });

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

        // Determine primitive type from spatial associations
        let primitive_type = self.determine_primitive_type(&spatial_associations);

        let feature = FeatureRecord {
            frid,
            foid,
            attributes,
            spatial_associations,
            information_associations,
            feature_associations,
            masks: Vec::new(),
            feature_code: None,
            primitive_type,
        };

        let id = RecordId::new(100, rcid);
        self.features.insert(id.key(), feature);
        Ok(())
    }

    /// Parse attributes from ATTR field
    fn parse_attributes(&self, data: &[u8], attrs: &mut Vec<Attribute>) -> Result<()> {
        let mut offset = 0;

        while offset + 5 < data.len() {
            // Check for terminator
            if data[offset] == FIELD_TERMINATOR {
                break;
            }

            // NATC(2) + ATIX(2) + PAIX(2) + ATIN(1) + ATVL(variable)
            let natc = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let atix = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
            let paix = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
            let _atin = data[offset + 6];
            offset += 7;

            // Read attribute value
            let (atvl, consumed) = read_string(&data[offset..])?;
            offset += consumed;

            attrs.push(Attribute {
                natc,
                atix,
                paix,
                atvl,
                value: None,
                code: None,
            });
        }

        Ok(())
    }

    /// Parse spatial associations from SPAS field
    fn parse_spatial_associations(&self, data: &[u8], assocs: &mut Vec<SpatialAssociation>) -> Result<()> {
        let mut offset = 0;

        while offset + 8 <= data.len() {
            if data[offset] == FIELD_TERMINATOR {
                break;
            }

            let rcnm = data[offset];
            let rcid = u32::from_le_bytes([
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
            ]);
            let ornt = data[offset + 5] as i8;
            let usag = data[offset + 6];
            let mask = data[offset + 7];

            assocs.push(SpatialAssociation {
                spatial_id: RecordId::new(rcnm, rcid),
                ornt,
                usag,
                mask,
            });

            offset += 8;
        }

        Ok(())
    }

    /// Parse information associations from INAS field
    fn parse_info_associations(&self, data: &[u8], assocs: &mut Vec<InformationAssociation>) -> Result<()> {
        let mut offset = 0;

        while offset + 9 <= data.len() {
            if data[offset] == FIELD_TERMINATOR {
                break;
            }

            let niac = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let narc = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
            let rcnm = data[offset + 4];
            let rcid = u32::from_le_bytes([
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
                data[offset + 8],
            ]);

            assocs.push(InformationAssociation {
                niac,
                narc,
                info_id: RecordId::new(rcnm, rcid),
            });

            offset += 9;
        }

        Ok(())
    }

    /// Parse feature associations from FASC field
    fn parse_feature_associations(&self, data: &[u8], assocs: &mut Vec<FeatureAssociation>) -> Result<()> {
        let mut offset = 0;

        while offset + 9 <= data.len() {
            if data[offset] == FIELD_TERMINATOR {
                break;
            }

            let nfac = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let narc = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
            let rcnm = data[offset + 4];
            let rcid = u32::from_le_bytes([
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
                data[offset + 8],
            ]);

            assocs.push(FeatureAssociation {
                nfac,
                narc,
                feature_id: RecordId::new(rcnm, rcid),
            });

            offset += 9;
        }

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
        let irid_field = dr.find_field(tags::IRID)
            .ok_or_else(|| S100Error::MissingField("IRID".into()))?;

        let data = irid_field.data_trimmed();
        if data.len() < 8 {
            return Err(S100Error::InvalidFieldData("IRID too short".into()));
        }

        let rcid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let nitc = u16::from_le_bytes([data[4], data[5]]);
        let rver = u16::from_le_bytes([data[6], data[7]]);
        let ruin = if data.len() > 8 { data[8] } else { 1 };

        let irid = IRID { rcid, nitc, rver, ruin };

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

        let id = RecordId::new(110, rcid);
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
            tracing::warn!(
                "Unmapped NFTC codes: {:?} (not in FTCS)",
                unmapped_nftc
            );
        }

        // Apply to information records
        for info in self.information.values_mut() {
            info.info_code = self
                .code_mappings
                .info_type_code(info.irid.nitc)
                .cloned();

            for attr in &mut info.attributes {
                attr.code = self.code_mappings.attribute_code(attr.natc).cloned();
            }
        }

        self.code_mappings.log_summary();
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

        // Build a lookup map from FC codes (lowercased for case-insensitive matching)
        let fc_codes_lower: HashMap<String, &String> = fc_feature_codes
            .iter()
            .map(|c| (c.to_lowercase(), c))
            .collect();

        // Also build a set of FC codes as-is for quick lookup
        let fc_codes_set: std::collections::HashSet<&String> = fc_feature_codes.iter().collect();

        // Try to find a matching FC code for a given data code
        let find_fc_code = |data_code: &str| -> Option<String> {
            // First, check exact match
            if fc_codes_set.contains(&data_code.to_string()) {
                return Some(data_code.to_string());
            }

            // Try case-insensitive match
            if let Some(fc_code) = fc_codes_lower.get(&data_code.to_lowercase()) {
                return Some((*fc_code).clone());
            }

            // Try prefix/suffix swap heuristic:
            // "BuoyCardinal" -> "CardinalBuoy", "BeaconLateral" -> "LateralBeacon"
            let prefixes = ["Buoy", "Beacon", "Restricted"];
            for prefix in prefixes {
                if data_code.starts_with(prefix) {
                    let suffix = &data_code[prefix.len()..];
                    let swapped = format!("{}{}", suffix, prefix);
                    if let Some(fc_code) = fc_codes_lower.get(&swapped.to_lowercase()) {
                        return Some((*fc_code).clone());
                    }
                }
            }

            // Try suffix-to-prefix swap:
            // "RestrictedAreaNavigational" -> "RestrictedArea" (drop suffix)
            let suffixes = ["Navigational", "Regulatory", "WarpingFacility"];
            for suffix in suffixes {
                if data_code.ends_with(suffix) {
                    let base = &data_code[..data_code.len() - suffix.len()];
                    if let Some(fc_code) = fc_codes_lower.get(&base.to_lowercase()) {
                        return Some((*fc_code).clone());
                    }
                }
            }

            // Special cases for compound names
            if data_code == "MooringWarpingFacility" {
                if let Some(fc_code) = fc_codes_lower.get("mooringarea") {
                    return Some((*fc_code).clone());
                }
            }

            None
        };

        // Normalize feature codes
        let mut normalized_count = 0;
        for feature in self.features.values_mut() {
            if let Some(ref data_code) = feature.feature_code {
                if !fc_codes_set.contains(data_code) {
                    if let Some(fc_code) = find_fc_code(data_code) {
                        tracing::debug!(
                            "Normalized feature code: {} -> {} (feature ID: {})",
                            data_code, fc_code, feature.frid.rcid
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
            if !fc_codes_set.contains(code) {
                if let Some(fc_code) = find_fc_code(code) {
                    ftcs_changes.push((*num, fc_code));
                }
            }
        }

        for (num, fc_code) in ftcs_changes {
            self.code_mappings.feature_types.num_to_str.insert(num, fc_code);
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
            self.features, self.information, self.points, self.multi_points, self.curves, self.surfaces
        )
    }
}
