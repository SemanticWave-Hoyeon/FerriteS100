//! S-101 Cell container
//!
//! Represents a complete S-101 ENC cell with all records.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ferrite_iso8211::{
    read_string, tags, MmapIso8211Parser, DR, FIELD_TERMINATOR, UNIT_TERMINATOR,
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
    // === S-101 Scale Information ===
    /// Compilation scale (e.g., 22000 for 1:22000)
    /// Used to determine feature visibility at different viewing scales
    pub compilation_scale: u32,
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
}

impl S101Cell {
    /// Load cell from file using memory-mapped I/O (zero-copy, 3-10x faster)
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        tracing::info!("Loading S-101 cell (mmap): {}", path.display());

        // Use memory-mapped parser for zero-copy file access
        let mut parser = MmapIso8211Parser::from_file(path)?;
        let (_ddr, records) = parser.read_all()?;

        let mut cell = S101Cell {
            file_path: path.to_path_buf(),
            dsid: DatasetIdentification::default(),
            code_mappings: DatasetCodeMappings::new(),
            coord_factor: 1.0,
            coord_factor_z: 0.01, // Default CMFZ=100, so factor = 1/100
            coord_origin_x: 0.0,
            coord_origin_y: 0.0,
            // S-101 scale defaults - will be parsed from DSID/DSPM
            compilation_scale: 22000, // Default 1:22000 (typical harbor scale)
            minimum_display_scale: None,
            maximum_display_scale: None,
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

        // Try to extract compilation scale from filename (S-101 naming convention)
        // Example: "101KR0022000.000" -> scale 22000
        cell.extract_scale_from_filename();

        // Apply code mappings to records
        cell.apply_code_mappings();

        // Separate interior rings by connectivity (S-100 10a-7.2.6)
        // The RIAS parser collects all interior curves into a single Vec,
        // but they may form multiple disjoint closed rings (holes).
        cell.separate_interior_rings();

        // Shrink excess memory after loading is complete
        cell.shrink_to_fit();

        tracing::info!(
            "Loaded cell: {} features, {} points, {} multi-points, {} curves, {} surfaces (scale 1:{})",
            cell.features.len(),
            cell.points.len(),
            cell.multi_points.len(),
            cell.curves.len(),
            cell.surfaces.len(),
            cell.compilation_scale
        );

        Ok(cell)
    }

    /// Process a single data record
    fn process_record(&mut self, dr: &DR) -> Result<()> {
        // Determine record type by first field tag
        if let Some(first_field) = dr.fields.first() {
            match first_field.tag.as_str() {
                tags::DSID | tags::DSPM => self.process_dsid(dr)?,
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
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ]);
                let dcoy = f64::from_le_bytes([
                    data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
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

        // Parse DSPM (Dataset Parameters) for compilation scale
        // S-101 DSPM format:
        // HDAT (b11/1 byte) - Horizontal Datum
        // VDAT (b11/1 byte) - Vertical Datum
        // SDAT (b11/1 byte) - Sounding Datum
        // CSCL (b14/4 bytes) - Compilation Scale denominator
        // DUNI (b11/1 byte) - Depth Unit
        // HUNI (b11/1 byte) - Height Unit
        // PUNI (b11/1 byte) - Positional Accuracy Unit
        // COUN (b11/1 byte) - Coordinate Units
        if let Some(dspm_field) = dr.find_field(tags::DSPM) {
            let data = dspm_field.data_trimmed();
            if data.len() >= 7 {
                // CSCL at offset 3, 4 bytes unsigned integer (big-endian per S-100)
                let cscl = u32::from_be_bytes([data[3], data[4], data[5], data[6]]);
                if cscl > 0 && cscl < 100_000_000 {
                    self.compilation_scale = cscl;
                    tracing::info!("DSPM: Compilation scale = 1:{}", cscl);
                } else {
                    // Try little-endian
                    let cscl_le = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
                    if cscl_le > 0 && cscl_le < 100_000_000 {
                        self.compilation_scale = cscl_le;
                        tracing::info!("DSPM: Compilation scale = 1:{} (LE)", cscl_le);
                    } else {
                        tracing::warn!(
                            "DSPM: Invalid compilation scale BE={} LE={}, raw bytes={:02X?}",
                            cscl,
                            cscl_le,
                            &data[3..7]
                        );
                    }
                }
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

        // Parse coordinates from C2IL or C3IL field
        let mut position = Coordinate::new(0.0, 0.0);

        // Look for C2IT (2D Integer Tuple) for points, not C2IL (which is for lists)
        if let Some(coord_field) = dr
            .find_field(tags::C2IT)
            .or_else(|| dr.find_field(tags::C3IT))
        {
            let coord_data = coord_field.data_trimmed();
            if coord_data.len() >= 8 {
                // S-101 stores coordinates as (YCOO, XCOO) = (latitude, longitude)
                let y = i32::from_le_bytes([
                    coord_data[0],
                    coord_data[1],
                    coord_data[2],
                    coord_data[3],
                ]);
                let x = i32::from_le_bytes([
                    coord_data[4],
                    coord_data[5],
                    coord_data[6],
                    coord_data[7],
                ]);
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
                            triplet_data[offset],
                            triplet_data[offset + 1],
                            triplet_data[offset + 2],
                            triplet_data[offset + 3],
                        ]);
                        let x = i32::from_le_bytes([
                            triplet_data[offset + 4],
                            triplet_data[offset + 5],
                            triplet_data[offset + 6],
                            triplet_data[offset + 7],
                        ]);
                        let z = i32::from_le_bytes([
                            triplet_data[offset + 8],
                            triplet_data[offset + 9],
                            triplet_data[offset + 10],
                            triplet_data[offset + 11],
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
            let y_raw = i32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);
            let x_raw = i32::from_le_bytes([
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);

            let x = x_raw as f64 * self.coord_factor + self.coord_origin_x;
            let y = y_raw as f64 * self.coord_factor + self.coord_origin_y;

            coords.push(Coordinate::new(x, y));

            offset += 8;
        }

        Ok(coords)
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
        let frid_field = dr
            .find_field(tags::FRID)
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

    /// Parse spatial associations from SPAS field.
    ///
    /// Each entry is RCNM(1) + RCID(4) + ORNT(1) + USAG(1) + MASK(1) = 8 bytes.
    /// The field is terminated by `FIELD_TERMINATOR` (0x1E), but some encoders
    /// pad the trailing bytes with zeros — without a stricter check those
    /// zero-padded chunks parsed as ghost associations with `rcnm=0,
    /// rcid=0xFFFFFF00`, which silently dropped at every `cell.points.get(&key)`
    /// call site. Half of the spatial-association records in the Portsmouth
    /// sample were these phantoms before this guard.
    ///
    /// Valid spatial RCNMs in S-101 are 110/115/120/125/130 (point /
    /// multi-point / curve / composite curve / surface). Anything else is
    /// rejected as padding — the entries don't reference a real record and
    /// only confuse downstream consumers (and LLMs, which is why the
    /// s101-mcp `--validate` self-check caught this).
    fn parse_spatial_associations(
        &self,
        data: &[u8],
        assocs: &mut Vec<SpatialAssociation>,
    ) -> Result<()> {
        const VALID_SPATIAL_RCNMS: [u8; 5] = [110, 115, 120, 125, 130];
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

            // Reject padding / no-record sentinels rather than emitting a
            // phantom association that no later lookup can resolve.
            if !VALID_SPATIAL_RCNMS.contains(&rcnm) {
                offset += 8;
                continue;
            }

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

    /// Parse MASK field records (S-100 4.8.3: mask indicators for spatial edges)
    /// Each record: RCNM(1) + RCID(4) + MIND(1) = 6 bytes
    fn parse_mask_records(&self, data: &[u8], masks: &mut Vec<MaskRecord>) -> Result<()> {
        let mut offset = 0;

        while offset + 6 <= data.len() {
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
            let mask_type = data[offset + 5];

            masks.push(MaskRecord {
                mask_type,
                spatial_id: RecordId::new(rcnm, rcid),
            });

            offset += 6;
        }

        Ok(())
    }

    /// Parse information associations from INAS field
    fn parse_info_associations(
        &self,
        data: &[u8],
        assocs: &mut Vec<InformationAssociation>,
    ) -> Result<()> {
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
    fn parse_feature_associations(
        &self,
        data: &[u8],
        assocs: &mut Vec<FeatureAssociation>,
    ) -> Result<()> {
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
        let irid_field = dr
            .find_field(tags::IRID)
            .ok_or_else(|| S100Error::MissingField("IRID".into()))?;

        let data = irid_field.data_trimmed();
        if data.len() < 8 {
            return Err(S100Error::InvalidFieldData("IRID too short".into()));
        }

        let rcid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let nitc = u16::from_le_bytes([data[4], data[5]]);
        let rver = u16::from_le_bytes([data[6], data[7]]);
        let ruin = if data.len() > 8 { data[8] } else { 1 };

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
            tracing::warn!("Unmapped NFTC codes: {:?} (not in FTCS)", unmapped_nftc);
        }

        // Apply to information records
        for info in self.information.values_mut() {
            info.info_code = self.code_mappings.info_type_code(info.irid.nitc).cloned();

            for attr in &mut info.attributes {
                attr.code = self.code_mappings.attribute_code(attr.natc).cloned();
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
        ) -> Option<(Coordinate, Coordinate)> {
            let key = oc.curve_id.key();
            if let Some(curve) = curves.get(&key) {
                let positions = curve.all_positions();
                if positions.is_empty() {
                    return None;
                }
                let (start, end) = (*positions.first().unwrap(), *positions.last().unwrap());
                if oc.orientation {
                    Some((start, end))
                } else {
                    Some((end, start))
                }
            } else if let Some(composite) = composites.get(&key) {
                let sub_curves = &composite.curves;
                if sub_curves.is_empty() {
                    return None;
                }
                let first_sub = sub_curves.first().unwrap();
                let last_sub = sub_curves.last().unwrap();
                let first_ep = curve_endpoints(
                    &OrientedCurve {
                        curve_id: first_sub.curve_id,
                        orientation: oc.orientation == first_sub.orientation,
                    },
                    curves,
                    composites,
                )?;
                let last_ep = curve_endpoints(
                    &OrientedCurve {
                        curve_id: last_sub.curve_id,
                        orientation: oc.orientation == last_sub.orientation,
                    },
                    curves,
                    composites,
                )?;
                if oc.orientation {
                    Some((first_ep.0, last_ep.1))
                } else {
                    Some((last_ep.1, first_ep.0))
                }
            } else {
                None
            }
        }

        fn coords_close(a: &Coordinate, b: &Coordinate) -> bool {
            (a.x - b.x).abs() < 1e-5 && (a.y - b.y).abs() < 1e-5
        }

        let curves_ref = &self.curves;
        let composites_ref = &self.composite_curves;

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
                .map(|oc| curve_endpoints(oc, curves_ref, composites_ref))
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

    /// Extract compilation scale from S-101 filename convention
    /// S-101 filename format: 101PPNNNSSSSSS where:
    ///   101 = product specification (S-101)
    ///   PP = producer code (2 letters)
    ///   NNN = navigational purpose (3 digits)
    ///   SSSSSS = cell identifier
    /// Navigational purpose maps to typical scales per S-101 Table 3-1
    fn extract_scale_from_filename(&mut self) {
        if let Some(filename) = self.file_path.file_stem().and_then(|s| s.to_str()) {
            // S-101 filenames start with "101" followed by 2-letter producer code
            // then 3-digit navigational purpose
            if filename.len() >= 8 && filename.starts_with("101") {
                // Producer code at position 3-4 (2 chars), navigational purpose at 5-7 (3 digits)
                let nav_purpose_str = &filename[5..8];
                if let Ok(nav_purpose) = nav_purpose_str.parse::<u32>() {
                    // S-101 Table 3-1: Navigational Purpose to typical compilation scale
                    let scale = match nav_purpose {
                        1 => 3_500_000, // Overview
                        2 => 350_000,   // General
                        3 => 90_000,    // Coastal
                        4 => 22_000,    // Approach
                        5 => 12_000,    // Harbour
                        6 => 4_000,     // Berthing
                        _ => {
                            tracing::debug!(
                                "Unknown navigational purpose {} in filename '{}', using default scale",
                                nav_purpose, filename
                            );
                            return;
                        }
                    };
                    self.compilation_scale = scale;
                    tracing::debug!(
                        "Filename '{}': navigational purpose {} -> scale 1:{}",
                        filename,
                        nav_purpose,
                        scale
                    );
                    return;
                }
            }

            tracing::debug!(
                "Could not extract navigational purpose from filename '{}', using default scale",
                filename
            );
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
