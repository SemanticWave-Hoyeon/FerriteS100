//! Owned S-100 Part 10a record transactions. This module never authenticates
//! datasets. Exchange-set identity and signature policy belong to the caller.
//! Ordered attribute and association commands stage within the transaction.
use crate::{Result, S100Error};
use ferrite_iso8211::{RawField, DR, FIELD_TERMINATOR};
use ferrite_kernel::sequence_update::{SequenceControl, UpdateInstruction};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

fn bad(s: impl Into<String>) -> S100Error {
    S100Error::InvalidRecord(s.into())
}
fn check(ok: bool, msg: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(bad(msg))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    pub name: u8,
    pub id: u32,
}
#[derive(Debug, Clone, Copy)]
pub struct UpdateRecordHeader {
    pub key: RecordKey,
    pub version: u16,
    pub instruction: UpdateInstruction,
}
impl UpdateRecordHeader {
    pub fn parse(field: &RawField) -> Result<Option<Self>> {
        let (name, len) = match field.tag.as_str() {
            "PRID" => (110, 8),
            "MRID" => (115, 8),
            "CRID" => (120, 8),
            "CCID" => (125, 8),
            "SRID" => (130, 8),
            "FRID" => (100, 10),
            "IRID" => (150, 10),
            _ => return Ok(None),
        };
        let d = field.data_trimmed();
        check(
            d.len() == len && d[0] == name,
            "Malformed record identifier",
        )?;
        let id = u32::from_le_bytes(d[1..5].try_into().unwrap());
        let version = u16::from_le_bytes(d[len - 3..len - 1].try_into().unwrap());
        check(
            id > 0 && id < u32::MAX && version > 0,
            "Invalid record identity/version",
        )?;
        let instruction = UpdateInstruction::parse(d[len - 1]).map_err(|e| bad(e.to_string()))?;
        Ok(Some(Self {
            key: RecordKey { name, id },
            version,
            instruction,
        }))
    }
}

/// Application limits bound retained payloads and every ordered-list result.
#[derive(Debug, Clone, Copy)]
pub struct UpdateLimits {
    pub max_records: usize,
    pub max_record_bytes: usize,
    pub max_dataset_bytes: usize,
    pub max_list_items: usize,
}
impl Default for UpdateLimits {
    fn default() -> Self {
        Self {
            max_records: 1_000_000,
            max_record_bytes: 64 * 1024 * 1024,
            max_dataset_bytes: 512 * 1024 * 1024,
            max_list_items: 1_000_000,
        }
    }
}
fn bytes(dr: &DR) -> Result<usize> {
    dr.fields.iter().try_fold(0usize, |s, f| {
        s.checked_add(f.data.len())
            .ok_or_else(|| bad("Record size overflow"))
    })
}
fn first_header(dr: &DR) -> Result<Option<UpdateRecordHeader>> {
    let f = dr.fields.first().ok_or_else(|| bad("Empty data record"))?;
    let header = UpdateRecordHeader::parse(f)?;
    check(
        !dr.fields.iter().skip(1).any(|f| {
            matches!(
                f.tag.as_str(),
                "PRID" | "MRID" | "CRID" | "CCID" | "SRID" | "FRID" | "IRID"
            )
        }),
        "Multiple record identifiers",
    )?;
    Ok(header)
}
fn has_control(dr: &DR) -> bool {
    dr.fields
        .iter()
        .any(|f| matches!(f.tag.as_str(), "COCC" | "SECC" | "CCOC"))
}

/// Only dirty records are copied while staging. Publish occurs after all
/// operations pass; each operation sees the preceding staged version.
pub struct S100RecordStore {
    metadata: Vec<Arc<DR>>,
    records: BTreeMap<RecordKey, Arc<DR>>,
    order: Vec<RecordKey>,
    deleted: HashSet<RecordKey>,
    payload_bytes: usize,
    limits: UpdateLimits,
}
impl S100RecordStore {
    pub fn from_base(records: Vec<DR>, limits: UpdateLimits) -> Result<Self> {
        let mut s = Self {
            metadata: Vec::new(),
            records: BTreeMap::new(),
            order: Vec::new(),
            deleted: HashSet::new(),
            payload_bytes: 0,
            limits,
        };
        check(
            records.len() <= limits.max_records,
            "Base record budget exceeded",
        )?;
        for dr in records {
            let n = bytes(&dr)?;
            check(
                n <= limits.max_record_bytes,
                "Base record byte budget exceeded",
            )?;
            s.payload_bytes = s
                .payload_bytes
                .checked_add(n)
                .ok_or_else(|| bad("Dataset size overflow"))?;
            check(
                s.payload_bytes <= limits.max_dataset_bytes,
                "Base dataset byte budget exceeded",
            )?;
            check(
                !has_control(&dr),
                "Update control encountered in base dataset",
            )?;
            if let Some(h) = first_header(&dr)? {
                check(
                    h.instruction == UpdateInstruction::Insert,
                    "Update record encountered in base dataset",
                )?;
                check(
                    !s.records.contains_key(&h.key),
                    "Duplicate base record identity",
                )?;
                s.order.push(h.key);
                s.records.insert(h.key, Arc::new(dr));
            } else {
                s.metadata.push(Arc::new(dr));
            }
        }
        Ok(s)
    }
    /// Replace the adapter's canonical metadata with checked byte accounting.
    /// Construct and validate the replacement before publishing any mutation.
    pub(crate) fn replace_code_metadata(&mut self, records: Vec<DR>) -> Result<()> {
        check(
            self.order
                .len()
                .checked_add(records.len())
                .is_some_and(|n| n <= self.limits.max_records),
            "Metadata identity history budget exceeded",
        )?;
        let old = self.metadata.iter().try_fold(0usize, |n, r| {
            n.checked_add(bytes(r)?)
                .ok_or_else(|| bad("Metadata size overflow"))
        })?;
        let mut added = 0usize;
        for r in &records {
            check(
                first_header(r)?.is_none(),
                "Data record in metadata replacement",
            )?;
            let size = bytes(r)?;
            check(
                size <= self.limits.max_record_bytes,
                "Metadata record byte budget exceeded",
            )?;
            added = added
                .checked_add(size)
                .ok_or_else(|| bad("Metadata size overflow"))?;
        }
        let total = self
            .payload_bytes
            .checked_sub(old)
            .and_then(|n| n.checked_add(added))
            .ok_or_else(|| bad("Metadata byte accounting overflow"))?;
        check(
            total <= self.limits.max_dataset_bytes,
            "Metadata dataset byte budget exceeded",
        )?;
        self.metadata = records.into_iter().map(Arc::new).collect();
        self.payload_bytes = total;
        Ok(())
    }
    pub fn record(&self, key: RecordKey) -> Option<&DR> {
        self.records.get(&key).map(AsRef::as_ref)
    }
    /// Borrow retained data records without cloning the materialized dataset.
    pub(crate) fn records(&self) -> impl Iterator<Item = (RecordKey, &DR)> {
        self.records
            .iter()
            .map(|(key, record)| (*key, record.as_ref()))
    }

    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }
    /// Inspect every retained raw association, including spatial INAS fields
    /// that older public spatial structs do not expose. A deleted referenced
    /// record must never become silently missing geometry or information.
    pub fn validate_references(&self) -> Result<()> {
        for record in self.records.values() {
            for f in &record.fields {
                let d = f.data_trimmed();
                if matches!(f.tag.as_str(), "CUCO" | "RIAS") {
                    crate::cell::validate_curve_association_field(d, f.tag == "RIAS")?;
                }
                if f.tag == "SPAS" {
                    // Share the cell parser's target/orientation/command checks.
                    crate::cell::validate_materialized_spas(d)?;
                }
                let (width, instruction, kind) = match f.tag.as_str() {
                    "PTAS" => (6, None, 110),
                    "CUCO" => (6, None, 120),
                    "RIAS" => (8, Some(7), 120),
                    "SPAS" => (15, Some(14), 0),
                    "MASK" => (7, Some(6), 0),
                    "INAS" | "FASC" => {
                        check(
                            d.len() >= 10 && d[9] == 1,
                            "Unmaterialized raw association instruction",
                        )?;
                        let name = if f.tag == "INAS" { 150 } else { 100 };
                        check(d[0] == name, "Invalid association target type")?;
                        let key = RecordKey {
                            name,
                            id: u32::from_le_bytes(d[1..5].try_into().unwrap()),
                        };
                        check(
                            self.records.contains_key(&key),
                            "Dangling raw information/feature association",
                        )?;
                        continue;
                    }
                    _ => continue,
                };
                check(d.len() % width == 0, "Truncated raw association tuple")?;
                for t in d.chunks_exact(width) {
                    if let Some(i) = instruction {
                        check(t[i] == 1, "Unmaterialized raw association instruction")?;
                    }
                    check(
                        match kind {
                            110 => t[0] == 110,
                            120 => matches!(t[0], 120 | 125),
                            _ => matches!(t[0], 110 | 115 | 120 | 125 | 130),
                        },
                        "Invalid raw spatial reference type",
                    )?;
                    if f.tag == "PTAS" {
                        check(matches!(t[5], 1..=3), "Invalid point topology indicator")?;
                    }
                    if matches!(f.tag.as_str(), "CUCO" | "RIAS") {
                        check(matches!(t[5], 1 | 2), "Invalid curve orientation")?;
                    }
                    let key = RecordKey {
                        name: t[0],
                        id: u32::from_le_bytes(t[1..5].try_into().unwrap()),
                    };
                    check(
                        self.records.contains_key(&key),
                        "Dangling raw spatial association",
                    )?;
                }
            }
        }
        Ok(())
    }
    /// S-101 feature object identifiers must be unique within the materialized
    /// dataset. The caller uses this only after product identity qualification.
    pub(crate) fn validate_s101_feature_ids(&self) -> Result<()> {
        let mut seen = HashSet::<[u8; 8]>::new();
        for (key, record) in &self.records {
            if key.name != 100 {
                continue;
            }
            let foid =
                one(&record.fields, "FOID")?.ok_or_else(|| bad("S-101 feature has no FOID"))?;
            let d = foid.data_trimmed();
            check(d.len() == 8, "Malformed S-101 FOID")?;
            let fidn = u32::from_le_bytes(d[2..6].try_into().unwrap());
            check(
                fidn > 0 && fidn < u32::MAX,
                "Invalid feature object identifier number",
            )?;
            check(
                seen.insert(d.try_into().unwrap()),
                "Duplicate S-101 feature object identity",
            )?;
        }
        Ok(())
    }
    /// Validate materialized attribute trees, including untouched base fields
    /// and attribute suffixes carried by feature/information associations.
    pub(crate) fn validate_s101_attribute_trees(&self) -> Result<()> {
        for record in self.records.values() {
            if record.fields.iter().any(|f| f.tag == "ATTR") {
                crate::attribute_updates::apply_attributes(
                    &record.fields,
                    &[],
                    self.limits.max_list_items,
                    self.limits.max_record_bytes,
                )?;
            }
            for f in record
                .fields
                .iter()
                .filter(|f| matches!(f.tag.as_str(), "INAS" | "FASC"))
            {
                let d = f.data_trimmed();
                check(d.len() >= 10, "Truncated materialized association")?;
                crate::attribute_updates::apply_attributes(
                    &[association_attributes(&d[10..])],
                    &[],
                    self.limits.max_list_items,
                    self.limits.max_record_bytes,
                )?;
            }
        }
        Ok(())
    }
    pub fn apply_records(&mut self, updates: &[DR]) -> Result<()> {
        check(
            updates.len() <= self.limits.max_records,
            "Update record budget exceeded",
        )?;
        let mut pending: BTreeMap<RecordKey, Option<Arc<DR>>> = BTreeMap::new();
        let mut inserted = Vec::new();
        let mut deleted = HashSet::new();
        let mut total = self.payload_bytes;
        for update in updates {
            check(
                bytes(update)? <= self.limits.max_record_bytes,
                "Update record byte budget exceeded",
            )?;
            let h = first_header(update)?
                .ok_or_else(|| bad("Metadata must be checked before record transaction"))?;
            let old = pending
                .get(&h.key)
                .map(|p| p.as_deref())
                .unwrap_or_else(|| self.record(h.key));
            let result = match h.instruction {
                UpdateInstruction::Insert => {
                    check(
                        old.is_none()
                            && !self.deleted.contains(&h.key)
                            && !deleted.contains(&h.key),
                        "Record already exists or identifier was deleted",
                    )?;
                    check(
                        h.version == 1 && !has_control(update),
                        "Inserted record must start at version one without controls",
                    )?;
                    validate_inserted(update, self.limits)?;
                    inserted.push(h.key);
                    Some(update.clone())
                }
                UpdateInstruction::Delete | UpdateInstruction::Modify => {
                    let old = old.ok_or_else(|| bad("Update target does not exist"))?;
                    let prev =
                        first_header(old)?.ok_or_else(|| bad("Missing target identifier"))?;
                    check(
                        prev.version.checked_add(1) == Some(h.version),
                        "Skipped, repeated, or overflowing record version",
                    )?;
                    if h.instruction == UpdateInstruction::Delete {
                        check(
                            update.fields.len() == 1,
                            "Deleted record must have identifier only",
                        )?;
                        deleted.insert(h.key);
                        None
                    } else {
                        Some(modified(old, update, self.limits)?)
                    }
                }
            };
            if let Some(old) = old {
                total = total
                    .checked_sub(bytes(old)?)
                    .ok_or_else(|| bad("Dataset byte accounting underflow"))?;
            }
            if let Some(dr) = result.as_ref() {
                let n = bytes(dr)?;
                check(
                    n <= self.limits.max_record_bytes,
                    "Result record byte budget exceeded",
                )?;
                total = total
                    .checked_add(n)
                    .ok_or_else(|| bad("Dataset size overflow"))?;
            }
            check(
                total <= self.limits.max_dataset_bytes,
                "Result dataset byte budget exceeded",
            )?;
            pending.insert(
                h.key,
                result.map(|mut dr| {
                    // Materialized fields represent the current record, not another
                    // update. The wire leader/directory are not serialization output.
                    let f = &mut dr.fields[0];
                    let len = f.data_trimmed().len();
                    f.data[len - 1] = 1;
                    Arc::new(dr)
                }),
            );
        }
        let count = self
            .records
            .len()
            .checked_add(inserted.len())
            .and_then(|n| n.checked_sub(deleted.len()))
            .ok_or_else(|| bad("Record count overflow"))?;
        check(
            count <= self.limits.max_records,
            "Result record budget exceeded",
        )?;
        // Deleted identities cannot be reused. Bound retained order/tombstone
        // history too, rather than bounding only currently live records.
        check(
            self.order
                .len()
                .checked_add(inserted.len())
                .and_then(|n| n.checked_add(self.metadata.len()))
                .is_some_and(|n| n <= self.limits.max_records),
            "Retained identity history budget exceeded",
        )?;
        for (key, record) in pending {
            if let Some(dr) = record {
                self.records.insert(key, dr);
            } else {
                self.records.remove(&key);
            }
        }
        self.order.extend(inserted);
        self.deleted.extend(deleted);
        self.payload_bytes = total;
        Ok(())
    }
    /// Field materialization only: leader/directory offsets must be regenerated
    /// by a future encoder before writing an ISO 8211 file.
    pub fn into_records(self) -> Vec<DR> {
        let mut out: Vec<DR> = self
            .metadata
            .into_iter()
            .map(|r| Arc::try_unwrap(r).unwrap_or_else(|r| (*r).clone()))
            .collect();
        let mut records = self.records;
        for key in self.order {
            if let Some(r) = records.remove(&key) {
                out.push(Arc::try_unwrap(r).unwrap_or_else(|r| (*r).clone()));
            }
        }
        out
    }
}

fn one<'a>(fields: &'a [RawField], tag: &str) -> Result<Option<&'a RawField>> {
    let mut iter = fields.iter().filter(|f| f.tag == tag);
    let f = iter.next();
    check(iter.next().is_none(), "Duplicate control/singleton field")?;
    Ok(f)
}
fn control(fields: &[RawField], tag: &str) -> Result<SequenceControl> {
    let f = one(fields, tag)?.ok_or_else(|| bad(format!("Missing {tag}")))?;
    let d = f.data_trimmed();
    check(d.len() == 5, "Malformed ordered-list control")?;
    Ok(SequenceControl {
        instruction: UpdateInstruction::parse(d[0]).map_err(|e| bad(e.to_string()))?,
        index: u16::from_le_bytes(d[1..3].try_into().unwrap()),
        count: u16::from_le_bytes(d[3..5].try_into().unwrap()),
    })
}
fn field(tag: &str, mut data: Vec<u8>) -> RawField {
    data.push(FIELD_TERMINATOR);
    RawField::new(tag.into(), data)
}
#[derive(Clone)]
struct Coordinates {
    tag: String,
    vcid: Option<u8>,
    tuples: Vec<[u8; 12]>,
}
fn coordinates(fields: &[RawField], max: usize) -> Result<Option<Coordinates>> {
    let mut result: Option<Coordinates> = None;
    for f in fields
        .iter()
        .filter(|f| matches!(f.tag.as_str(), "C2IL" | "C3IL"))
    {
        let d = f.data_trimmed();
        let three = f.tag == "C3IL";
        check(!three || !d.is_empty(), "Missing VCID")?;
        let vcid = if three { Some(d[0]) } else { None };
        let payload = &d[usize::from(three)..];
        let width = if three { 12 } else { 8 };
        check(payload.len() % width == 0, "Truncated coordinate tuple")?;
        let r = result.get_or_insert_with(|| Coordinates {
            tag: f.tag.clone(),
            vcid,
            tuples: Vec::new(),
        });
        check(
            r.tag == f.tag && r.vcid == vcid,
            "Mixed coordinate encoding or vertical CRS",
        )?;
        let n = r
            .tuples
            .len()
            .checked_add(payload.len() / width)
            .ok_or_else(|| bad("Tuple count overflow"))?;
        check(n <= max, "Coordinate item budget exceeded")?;
        r.tuples
            .try_reserve(payload.len() / width)
            .map_err(|e| bad(e.to_string()))?;
        for tuple in payload.chunks_exact(width) {
            let mut t = [0; 12];
            t[..width].copy_from_slice(tuple);
            r.tuples.push(t);
        }
    }
    Ok(result)
}
fn coordinate_field(c: Coordinates) -> RawField {
    let width = if c.vcid.is_some() { 12 } else { 8 };
    let mut data = Vec::with_capacity(c.tuples.len() * width + usize::from(c.vcid.is_some()) + 1);
    if let Some(v) = c.vcid {
        data.push(v);
    }
    for t in c.tuples {
        data.extend_from_slice(&t[..width]);
    }
    field(&c.tag, data)
}
fn changed_coordinates(
    old: &[RawField],
    update: &[RawField],
    limits: UpdateLimits,
) -> Result<RawField> {
    let mut a = coordinates(old, limits.max_list_items)?
        .ok_or_else(|| bad("Target coordinate stream missing"))?;
    let b = coordinates(update, limits.max_list_items)?;
    let ctrl = control(update, "COCC")?;
    if let Some(b) = b.as_ref() {
        check(
            a.tag == b.tag && a.vcid == b.vcid,
            "Update coordinate encoding or vertical CRS differs",
        )?;
    }
    let payload = b.as_ref().map(|b| b.tuples.as_slice()).unwrap_or(&[]);
    a.tuples = ctrl
        .applied(&a.tuples, payload, limits.max_list_items)
        .map_err(|e| bad(e.to_string()))?;
    Ok(coordinate_field(a))
}
fn components(fields: &[RawField], max: usize) -> Result<Vec<[u8; 6]>> {
    let mut out = Vec::new();
    for f in fields.iter().filter(|f| f.tag == "CUCO") {
        let d = f.data_trimmed();
        check(d.len().is_multiple_of(6), "Truncated curve component")?;
        check(
            out.len().checked_add(d.len() / 6).is_some_and(|n| n <= max),
            "Component item budget exceeded",
        )?;
        for c in d.as_chunks::<6>().0.iter() {
            check(
                matches!(c[0], 120 | 125) && matches!(c[5], 1 | 2),
                "Invalid component reference/orientation",
            )?;
            out.push(*c);
        }
    }
    Ok(out)
}
fn segments(fields: &[RawField]) -> Result<Vec<Vec<RawField>>> {
    let mut out: Vec<Vec<RawField>> = Vec::new();
    for f in fields {
        if f.tag == "SEGH" {
            check(
                f.data_trimmed() == [4],
                "S-101 requires loxodromic segment header",
            )?;
            out.push(vec![f.clone()]);
        } else if matches!(f.tag.as_str(), "COCC" | "C2IL" | "C3IL") {
            out.last_mut()
                .ok_or_else(|| bad("Segment data precedes header"))?
                .push(f.clone());
        }
    }
    Ok(out)
}
fn allowed(update: &DR, tags: &[&str]) -> Result<()> {
    check(
        update.fields.iter().all(|f| tags.contains(&f.tag.as_str())),
        "Unsupported partial record field; transaction rejected",
    )
}
fn insertion_attributes(data: &[u8]) -> Result<()> {
    let mut p = 0;
    while p < data.len() {
        check(data.len() - p >= 7, "Truncated inserted attribute")?;
        check(
            data[p + 6] == 1,
            "Inserted record contains non-insert attribute instruction",
        )?;
        p += 7;
        p += data[p..]
            .iter()
            .position(|b| *b == 0x1f || *b == 0)
            .map(|n| n + 1)
            .unwrap_or(data.len() - p);
    }
    Ok(())
}
fn validate_inserted(dr: &DR, limits: UpdateLimits) -> Result<()> {
    let tag = dr.fields[0].tag.as_str();
    let tags: &[&str] = match tag {
        "PRID" => &["PRID", "INAS", "C2IT", "C3IT"],
        "MRID" => &["MRID", "INAS", "C2IL", "C3IL"],
        "CRID" => &["CRID", "INAS", "PTAS", "SEGH", "C2IL", "C3IL"],
        "CCID" => &["CCID", "INAS", "CUCO"],
        "SRID" => &["SRID", "INAS", "RIAS"],
        "FRID" => &["FRID", "FOID", "ATTR", "INAS", "FASC", "SPAS", "MASK"],
        "IRID" => &["IRID", "ATTR", "INAS"],
        _ => return Err(bad("Unsupported inserted record type")),
    };
    allowed(dr, tags)?;
    if dr.fields.iter().any(|f| f.tag == "ATTR") {
        crate::attribute_updates::apply_attributes(
            &dr.fields,
            &[],
            limits.max_list_items,
            limits.max_record_bytes,
        )?;
    }
    for f in &dr.fields {
        let d = f.data_trimmed();
        match f.tag.as_str() {
            "ATTR" => insertion_attributes(d)?,
            "INAS" | "FASC" => {
                check(
                    d.len() >= 10 && d[9] == 1,
                    "Invalid inserted association instruction",
                )?;
                insertion_attributes(&d[10..])?;
                crate::attribute_updates::apply_attributes(
                    &[association_attributes(&d[10..])],
                    &[],
                    limits.max_list_items,
                    limits.max_record_bytes,
                )?;
            }
            "SPAS" | "RIAS" | "MASK" => {
                let width = match f.tag.as_str() {
                    "SPAS" => 15,
                    "RIAS" => 8,
                    _ => 7,
                };
                check(
                    d.len() % width == 0 && d.chunks_exact(width).all(|t| t[width - 1] == 1),
                    "Inserted record contains non-insert association instruction",
                )?;
            }
            _ => {}
        }
    }
    match tag {
        "PRID" => {
            let f: Vec<_> = dr
                .fields
                .iter()
                .filter(|f| f.tag == "C2IT" || f.tag == "C3IT")
                .collect();
            check(
                f.len() == 1
                    && f[0].data_trimmed().len() == if f[0].tag == "C2IT" { 8 } else { 13 },
                "Inserted point needs one complete coordinate",
            )?;
        }
        "MRID" => {
            check(
                coordinates(&dr.fields, limits.max_list_items)?.is_some(),
                "Inserted multi-point needs coordinates",
            )?;
        }
        "CRID" => {
            let s = segments(&dr.fields)?;
            check(!s.is_empty(), "Inserted curve needs segment header")?;
            for s in &s {
                check(
                    coordinates(s, limits.max_list_items)?.is_some(),
                    "Inserted segment needs coordinates",
                )?;
            }
        }
        "CCID" => {
            components(&dr.fields, limits.max_list_items)?;
        }
        _ => {}
    }
    Ok(())
}
fn modified_attributes(out: &mut DR, update: &DR, limits: UpdateLimits) -> Result<()> {
    if update.fields.iter().any(|f| f.tag == "ATTR") {
        let attrs = crate::attribute_updates::apply_attributes(
            &out.fields,
            &update.fields,
            limits.max_list_items,
            limits.max_record_bytes,
        )?;
        out.fields.retain(|f| f.tag != "ATTR");
        out.fields.push(attrs);
    }
    Ok(())
}
fn association_attributes(data: &[u8]) -> RawField {
    field("ATTR", data.to_vec())
}
fn modified_associations(out: &mut DR, update: &DR, tag: &str, limits: UpdateLimits) -> Result<()> {
    let changes: Vec<_> = update.fields.iter().filter(|f| f.tag == tag).collect();
    if changes.is_empty() {
        return Ok(());
    }
    let mut current: Vec<_> = out
        .fields
        .iter()
        .filter(|f| f.tag == tag)
        .cloned()
        .collect();
    for f in changes {
        let d = f.data_trimmed();
        check(d.len() >= 10, "Truncated association update")?;
        check(matches!(d[9], 1..=3), "Invalid association instruction")?;
        let matches: Vec<_> = current
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                let old = f.data_trimmed();
                old.len() >= 10 && old[..9] == d[..9]
            })
            .map(|(i, _)| i)
            .collect();
        check(matches.len() <= 1, "Ambiguous association identity")?;
        match d[9] {
            1 => {
                check(matches.is_empty(), "Duplicate inserted association")?;
                let attrs = crate::attribute_updates::apply_attributes(
                    &[association_attributes(&d[10..])],
                    &[],
                    limits.max_list_items,
                    limits.max_record_bytes,
                )?;
                let mut data = d[..10].to_vec();
                data.extend_from_slice(attrs.data_trimmed());
                current.push(field(tag, data));
            }
            2 => {
                check(d.len() == 10, "Deleted association has attribute payload")?;
                let i = *matches
                    .first()
                    .ok_or_else(|| bad("Association delete target missing"))?;
                current.remove(i);
            }
            3 => {
                let i = *matches
                    .first()
                    .ok_or_else(|| bad("Association modify target missing"))?;
                let old = current[i].data_trimmed();
                let attrs = crate::attribute_updates::apply_attributes(
                    &[association_attributes(&old[10..])],
                    &[association_attributes(&d[10..])],
                    limits.max_list_items,
                    limits.max_record_bytes,
                )?;
                let mut data = d[..10].to_vec();
                data[9] = 1;
                data.extend_from_slice(attrs.data_trimmed());
                current[i] = field(tag, data);
            }
            _ => unreachable!(),
        }
        check(
            current.len() <= limits.max_list_items,
            "Association item budget exceeded",
        )?;
    }
    out.fields.retain(|f| f.tag != tag);
    out.fields.extend(current);
    Ok(())
}
fn modified_fixed_associations(
    out: &mut DR,
    update: &DR,
    tag: &str,
    width: usize,
    limits: UpdateLimits,
) -> Result<()> {
    let changes: Vec<_> = update.fields.iter().filter(|f| f.tag == tag).collect();
    if changes.is_empty() {
        return Ok(());
    }
    let mut current = Vec::<Vec<u8>>::new();
    for f in out.fields.iter().filter(|f| f.tag == tag) {
        let d = f.data_trimmed();
        check(d.len() % width == 0, "Truncated base spatial association")?;
        for t in d.chunks_exact(width) {
            check(t[width - 1] == 1, "Unmaterialized base spatial association")?;
            current.push(t.to_vec());
        }
    }
    for f in changes {
        let d = f.data_trimmed();
        check(d.len() % width == 0, "Truncated spatial association update")?;
        for t in d.chunks_exact(width) {
            check(
                matches!(t[width - 1], 1 | 2),
                "Spatial associations only support Insert/Delete",
            )?;
            let exact: Vec<_> = current
                .iter()
                .enumerate()
                .filter(|(_, c)| c[..width - 1] == t[..width - 1])
                .map(|(i, _)| i)
                .collect();
            if t[width - 1] == 1 {
                check(exact.is_empty(), "Duplicate spatial association insertion")?;
                let mut item = t.to_vec();
                item[width - 1] = 1;
                current.push(item);
            } else {
                let i = if exact.len() == 1 {
                    exact[0]
                } else {
                    check(exact.is_empty(), "Ambiguous spatial association deletion")?;
                    // Published SHOM updates use NULL ORNT for deletion while
                    // the retained SPAS has forward ORNT. Resolve only a unique
                    // record reference with exactly matching scale qualifiers.
                    // Multiple candidate references remain a hard error.
                    check(
                        tag == "SPAS" && t[5] == 255,
                        "Spatial association delete target missing",
                    )?;
                    let candidates: Vec<_> = current
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c[..5] == t[..5] && c[6..14] == t[6..14])
                        .map(|(i, _)| i)
                        .collect();
                    check(
                        candidates.len() == 1,
                        "Ambiguous/missing NULL-orientation SPAS deletion",
                    )?;
                    candidates[0]
                };
                current.remove(i);
            }
            check(
                current.len() <= limits.max_list_items,
                "Spatial association item budget exceeded",
            )?;
        }
    }
    let size = current
        .len()
        .checked_mul(width)
        .ok_or_else(|| bad("Spatial association size overflow"))?;
    check(
        size < limits.max_record_bytes,
        "Spatial association byte budget exceeded",
    )?;
    let mut data = Vec::with_capacity(size);
    for item in current {
        data.extend(item);
    }
    out.fields.retain(|f| f.tag != tag);
    out.fields.push(field(tag, data));
    Ok(())
}
fn modified(old: &DR, update: &DR, limits: UpdateLimits) -> Result<DR> {
    let tag = update.fields[0].tag.as_str();
    let mut out = old.clone();
    match tag {
        "PRID"=>{
            allowed(update,&["PRID","INAS","C2IT","C3IT"])?;
            modified_associations(&mut out,update,"INAS",limits)?;
            if update.fields.iter().all(|f|f.tag=="INAS"||f.tag=="PRID"){out.fields[0]=update.fields[0].clone();return Ok(out);}
            let coords:Vec<_>=update.fields.iter().filter(|f|f.tag=="C2IT"||f.tag=="C3IT").collect();
            check(coords.len()==1,"Point modification needs one coordinate tuple")?;
            let f=coords[0];let before=one(&old.fields,&f.tag)?.ok_or_else(||bad("Point coordinate type differs"))?;
            let n=if f.tag=="C2IT"{8}else{13};check(f.data_trimmed().len()==n && before.data_trimmed().len()==n,"Truncated point tuple")?;
            check(n==8||f.data_trimmed()[0]==before.data_trimmed()[0],"Point vertical CRS differs")?;
            out.fields.retain(|f|f.tag!="C2IT"&&f.tag!="C3IT");out.fields.push(f.clone());
        }
        "MRID"=>{
            allowed(update,&["MRID","INAS","COCC","C2IL","C3IL"])?;
            modified_associations(&mut out,update,"INAS",limits)?;
            if update.fields.iter().all(|f|f.tag=="INAS"||f.tag=="MRID"){out.fields[0]=update.fields[0].clone();return Ok(out);}
            let f=changed_coordinates(&old.fields,&update.fields,limits)?;
            out.fields.retain(|f|f.tag!="C2IL"&&f.tag!="C3IL");out.fields.push(f);
        }
        "CCID"=>{
            allowed(update,&["CCID","INAS","CCOC","CUCO"])?;
            modified_associations(&mut out,update,"INAS",limits)?;
            if update.fields.iter().all(|f|f.tag=="INAS"||f.tag=="CCID"){out.fields[0]=update.fields[0].clone();return Ok(out);}
            let a=components(&old.fields,limits.max_list_items)?;let b=components(&update.fields,limits.max_list_items)?;
            let items=control(&update.fields,"CCOC")?.applied(&a,&b,limits.max_list_items).map_err(|e|bad(e.to_string()))?;
            let d:Vec<u8>=items.into_iter().flatten().collect();out.fields.retain(|f|f.tag!="CUCO");out.fields.push(field("CUCO",d));
        }
        "CRID"=>{
            allowed(update,&["CRID","INAS","SECC","SEGH","COCC","C2IL","C3IL"])?;
            allowed(old,&["CRID","INAS","INAS","PTAS","SEGH","C2IL","C3IL"])?;
            modified_associations(&mut out,update,"INAS",limits)?;
            if update.fields.iter().all(|f|f.tag=="INAS"||f.tag=="CRID"){out.fields[0]=update.fields[0].clone();return Ok(out);}
            let mut a=segments(&old.fields)?;let b=segments(&update.fields)?;let ctrl=control(&update.fields,"SECC")?;
            let range=ctrl.addressed_range(a.len(),b.len(),limits.max_list_items).map_err(|e|bad(e.to_string()))?;
            match ctrl.instruction {
                UpdateInstruction::Modify=>{
                    for (i,s) in b.iter().enumerate(){let f=changed_coordinates(&a[range.start+i],s,limits)?;
                        a[range.start+i]=vec![s[0].clone(),f];}
                }
                UpdateInstruction::Insert=>{
                    for s in &b {check(!s.iter().any(|f|f.tag=="COCC"),"Inserted segment has coordinate update control")?;
                        check(coordinates(s,limits.max_list_items)?.is_some(),"Inserted segment missing coordinates")?;}
                    a=ctrl.applied(&a,&b,limits.max_list_items).map_err(|e|bad(e.to_string()))?;
                }
                UpdateInstruction::Delete=>{a=ctrl.applied(&a,&[],limits.max_list_items).map_err(|e|bad(e.to_string()))?;}
            }
            out.fields.retain(|f|!matches!(f.tag.as_str(),"SEGH"|"COCC"|"C2IL"|"C3IL"));
            out.fields.extend(a.into_iter().flatten());
        }
        "FRID"|"IRID"=>{
            allowed(update,if tag=="FRID"{&["FRID","FOID","ATTR","INAS","FASC","SPAS","MASK"]}else{&["IRID","ATTR","INAS"]})?;
            if let Some(foid)=one(&update.fields,"FOID")? {check(foid.data_trimmed().len()==8,"Malformed feature object identifier")?;out.fields.retain(|f|f.tag!="FOID");out.fields.push(foid.clone());}
            modified_attributes(&mut out,update,limits)?;
            modified_associations(&mut out,update,"INAS",limits)?;
            if tag=="FRID" {modified_associations(&mut out,update,"FASC",limits)?;modified_fixed_associations(&mut out,update,"SPAS",15,limits)?;modified_fixed_associations(&mut out,update,"MASK",7,limits)?;}
        }
        "SRID"=>{allowed(update,&["SRID","INAS","RIAS"])?;modified_associations(&mut out,update,"INAS",limits)?;modified_fixed_associations(&mut out,update,"RIAS",8,limits)?;}
        _=>return Err(bad("Partial feature/information/surface modifications not implemented; transaction rejected")),
    }
    out.fields[0] = update.fields[0].clone();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn retained_invalid_ring_usage_and_spatial_direction_are_rejected() {
        let curve = dr(vec![id("CRID", 1, 1, 1)]);
        for (tag, tuple) in [
            ("RIAS", vec![120, 1, 0, 0, 0, 1, 0, 1]),
            ("RIAS", vec![120, 1, 0, 0, 0, 1, 3, 1]),
            ("SPAS", vec![120, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
        ] {
            let record = if tag == "RIAS" {
                dr(vec![id("SRID", 2, 1, 1), field(tag, tuple)])
            } else {
                let mut feature = vec![100, 2, 0, 0, 0, 1, 0, 1, 0, 1];
                dr(vec![
                    field("FRID", std::mem::take(&mut feature)),
                    field(tag, tuple),
                ])
            };
            let store =
                S100RecordStore::from_base(vec![curve.clone(), record], UpdateLimits::default())
                    .unwrap();
            assert!(store.validate_references().is_err());
        }
    }
    #[test]
    fn metadata_replacement_budget_failure_keeps_store_unchanged() {
        let metadata = dr(vec![
            field("DSID", vec![10, 1, 0, 0, 0]),
            field("FTCS", b"Old\x1f\x01\x00".to_vec()),
        ]);
        let limits = UpdateLimits {
            max_dataset_bytes: bytes(&metadata).unwrap() + 4,
            ..UpdateLimits::default()
        };
        let mut store = S100RecordStore::from_base(vec![metadata.clone()], limits).unwrap();
        let old = store.payload_bytes();
        let large = dr(vec![
            field("DSID", vec![10, 1, 0, 0, 0]),
            field("FTCS", b"MuchLongerNewName\x1f\x01\x00".to_vec()),
        ]);
        assert!(store.replace_code_metadata(vec![large]).is_err());
        assert_eq!(store.payload_bytes(), old);
        assert_eq!(store.metadata[0].fields[1].data, metadata.fields[1].data);
        let smaller = dr(vec![field("DSID", vec![10, 1, 0, 0, 0])]);
        let expected = bytes(&smaller).unwrap();
        store.replace_code_metadata(vec![smaller]).unwrap();
        assert_eq!(store.payload_bytes(), expected);
    }
    fn attrs(values: &[(u16, u16, u16, u8, &str)]) -> RawField {
        let mut data = Vec::new();
        for &(c, i, p, o, v) in values {
            data.extend(c.to_le_bytes());
            data.extend(i.to_le_bytes());
            data.extend(p.to_le_bytes());
            data.push(o);
            data.extend(v.as_bytes());
            data.push(0x1f);
        }
        field("ATTR", data)
    }
    fn assoc(tag: &str, target: u32, role: u16, op: u8, attribute: &RawField) -> RawField {
        let mut d = vec![if tag == "INAS" { 150 } else { 100 }];
        d.extend(target.to_le_bytes());
        d.extend(1u16.to_le_bytes());
        d.extend(role.to_le_bytes());
        d.push(op);
        d.extend(attribute.data_trimmed());
        field(tag, d)
    }
    fn spas(target: u32, orientation: u8, op: u8) -> RawField {
        let mut d = vec![130];
        d.extend(target.to_le_bytes());
        d.push(orientation);
        d.extend(u32::MAX.to_le_bytes());
        d.extend(0u32.to_le_bytes());
        d.push(op);
        field("SPAS", d)
    }
    #[test]
    fn inserted_attribute_trees_reject_bad_parent_utf8_index_and_termination() {
        let mut invalid = vec![1, 0, 1, 0, 0, 0, 1, 0xff, 0x1f];
        let bad_values = vec![
            vec![1, 0, 0, 0, 0, 0, 1, 0x1f],
            vec![1, 0, 1, 0, 1, 0, 1, 0x1f],
            vec![1, 0, 1, 0, 0, 0, 1],
            std::mem::take(&mut invalid),
        ];
        for value in bad_values {
            let mut store = S100RecordStore::from_base(vec![], UpdateLimits::default()).unwrap();
            let update = dr(vec![id("FRID", 1, 1, 1), field("ATTR", value.clone())]);
            assert!(store.apply_records(&[update]).is_err());
            assert_eq!(store.payload_bytes(), 0);
            let attribute = field("ATTR", value);
            let update = dr(vec![
                id("FRID", 1, 1, 1),
                assoc("FASC", 2, 1, 1, &attribute),
            ]);
            assert!(store.apply_records(&[update]).is_err());
            assert_eq!(store.payload_bytes(), 0);
        }
    }
    #[test]
    fn association_modify_preserves_omitted_children_and_other_roles() {
        let a = attrs(&[(3, 1, 0, 1, ""), (4, 1, 1, 1, "eng"), (5, 1, 1, 1, "Old")]);
        let b = attrs(&[(8, 1, 0, 1, "kept")]);
        let base = dr(vec![
            id("FRID", 1, 1, 1),
            assoc("FASC", 2, 1, 1, &a),
            assoc("FASC", 2, 2, 1, &b),
        ]);
        let change = attrs(&[(3, 1, 0, 3, ""), (5, 1, 1, 3, "New")]);
        let update = dr(vec![id("FRID", 1, 2, 3), assoc("FASC", 2, 1, 3, &change)]);
        let out = modified(&base, &update, UpdateLimits::default()).unwrap();
        let fs: Vec<_> = out.fields.iter().filter(|f| f.tag == "FASC").collect();
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[1].data, base.fields[2].data);
        let expected = attrs(&[(3, 1, 0, 1, ""), (4, 1, 1, 1, "eng"), (5, 1, 1, 1, "New")]);
        assert_eq!(&fs[0].data_trimmed()[10..], expected.data_trimmed());
        assert_eq!(fs[0].data_trimmed()[9], 1);
    }
    #[test]
    fn spas_null_orientation_delete_unique_reference_and_ambiguity_reject() {
        let base = dr(vec![id("FRID", 1, 1, 1), spas(25, 1, 1)]);
        let mut changes = spas(26, 1, 1).data_trimmed().to_vec();
        changes.extend(spas(25, 255, 2).data_trimmed());
        let update = dr(vec![id("FRID", 1, 2, 3), field("SPAS", changes)]);
        let out = modified(&base, &update, UpdateLimits::default()).unwrap();
        assert_eq!(
            one(&out.fields, "SPAS").unwrap().unwrap().data,
            spas(26, 1, 1).data
        );
        let ambiguous = dr(vec![id("FRID", 1, 1, 1), spas(25, 1, 1), spas(25, 2, 1)]);
        assert!(modified(&ambiguous, &update, UpdateLimits::default()).is_err());
        let illegal = dr(vec![id("FRID", 1, 2, 3), spas(25, 1, 3)]);
        assert!(modified(&base, &illegal, UpdateLimits::default()).is_err());
    }
    #[test]
    fn attribute_late_error_preserves_entire_record_transaction() {
        let base = dr(vec![
            id("FRID", 1, 1, 1),
            attrs(&[(3, 1, 0, 1, ""), (4, 1, 1, 1, "eng"), (5, 1, 1, 1, "Old")]),
        ]);
        let mut store =
            S100RecordStore::from_base(vec![base.clone()], UpdateLimits::default()).unwrap();
        let size = store.payload_bytes();
        let valid = dr(vec![
            id("FRID", 1, 2, 3),
            attrs(&[(3, 1, 0, 3, ""), (5, 1, 1, 3, "New")]),
        ]);
        let invalid = dr(vec![
            id("FRID", 1, 3, 3),
            attrs(&[(3, 1, 0, 2, ""), (4, 1, 1, 3, "x")]),
        ]);
        assert!(store.apply_records(&[valid, invalid]).is_err());
        assert_eq!(store.payload_bytes(), size);
        let retained = store.record(RecordKey { name: 100, id: 1 }).unwrap();
        assert_eq!(retained.fields[1].data, base.fields[1].data);
        assert_eq!(retained.fields[0].data, base.fields[0].data);
    }
    #[test]
    fn final_foid_uniqueness_and_missing_foid_are_checked() {
        let mut d = vec![1, 0];
        d.extend(1u32.to_le_bytes());
        d.extend(0u16.to_le_bytes());
        let identity = field("FOID", d);
        let a = dr(vec![id("FRID", 1, 1, 1), identity.clone()]);
        let b = dr(vec![id("FRID", 2, 1, 1), identity]);
        let store = S100RecordStore::from_base(vec![a, b], UpdateLimits::default()).unwrap();
        assert!(store.validate_s101_feature_ids().is_err());
        let store = S100RecordStore::from_base(
            vec![dr(vec![id("FRID", 3, 1, 1)])],
            UpdateLimits::default(),
        )
        .unwrap();
        assert!(store.validate_s101_feature_ids().is_err());
    }
    fn id(tag: &str, rcid: u32, version: u16, op: u8) -> RawField {
        let name = match tag {
            "PRID" => 110,
            "MRID" => 115,
            "CRID" => 120,
            "CCID" => 125,
            "SRID" => 130,
            "FRID" => 100,
            _ => unreachable!(),
        };
        let mut d = vec![name];
        d.extend(rcid.to_le_bytes());
        if tag == "FRID" {
            d.extend(1u16.to_le_bytes());
        }
        d.extend(version.to_le_bytes());
        d.push(op);
        field(tag, d)
    }
    fn ctrl(tag: &str, op: u8, index: u16, count: u16) -> RawField {
        let mut d = vec![op];
        d.extend(index.to_le_bytes());
        d.extend(count.to_le_bytes());
        field(tag, d)
    }
    fn c3(vcid: u8, values: &[i32]) -> RawField {
        let mut d = vec![vcid];
        for &n in values {
            d.extend(n.to_le_bytes());
            d.extend((n + 1).to_le_bytes());
            d.extend((-n).to_le_bytes());
        }
        field("C3IL", d)
    }
    fn c2(values: &[i32]) -> RawField {
        let mut d = Vec::new();
        for n in values {
            d.extend(n.to_le_bytes());
            d.extend(n.to_le_bytes());
        }
        field("C2IL", d)
    }
    fn values(dr: &DR) -> Vec<i32> {
        coordinates(&dr.fields, 100)
            .unwrap()
            .unwrap()
            .tuples
            .iter()
            .map(|t| i32::from_le_bytes(t[..4].try_into().unwrap()))
            .collect()
    }
    #[test]
    fn repeated_coordinate_fields_latest_indices_and_binary_bits() {
        let key = RecordKey { name: 115, id: 1 };
        let mut store = S100RecordStore::from_base(
            vec![dr(vec![
                id("MRID", 1, 1, 1),
                c3(7, &[30]),
                c3(7, &[31, 40]),
            ])],
            UpdateLimits::default(),
        )
        .unwrap();
        let updates = vec![
            dr(vec![
                id("MRID", 1, 2, 3),
                ctrl("COCC", 1, 2, 2),
                c3(7, &[50]),
                c3(7, &[60]),
            ]),
            dr(vec![id("MRID", 1, 3, 3), ctrl("COCC", 2, 3, 2)]),
            dr(vec![
                id("MRID", 1, 4, 3),
                ctrl("COCC", 3, 2, 1),
                c3(7, &[70]),
            ]),
        ];
        store.apply_records(&updates).unwrap();
        let result = store.record(key).unwrap();
        assert_eq!(values(result), [30, 70, 40]);
        assert_eq!(first_header(result).unwrap().unwrap().version, 4);
        let c = coordinates(&result.fields, 100).unwrap().unwrap();
        assert_eq!(c.vcid, Some(7));
        assert_eq!(&c.tuples[0][8..12], &(-30i32).to_le_bytes());
    }
    #[test]
    fn nested_segment_modification_is_not_whole_segment_replacement() {
        let key = RecordKey { name: 120, id: 9 };
        let mut store = S100RecordStore::from_base(
            vec![dr(vec![
                id("CRID", 9, 5, 1),
                field("PTAS", vec![110, 1, 0, 0, 0, 1]),
                field("SEGH", vec![4]),
                c2(&[10]),
                c2(&[20]),
                field("SEGH", vec![4]),
                c2(&[30, 40, 50]),
            ])],
            UpdateLimits::default(),
        )
        .unwrap();
        store
            .apply_records(&[dr(vec![
                id("CRID", 9, 6, 3),
                ctrl("SECC", 3, 2, 1),
                field("SEGH", vec![4]),
                ctrl("COCC", 2, 2, 1),
            ])])
            .unwrap();
        let r = store.record(key).unwrap();
        let s = segments(&r.fields).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(coordinates(&s[0], 100).unwrap().unwrap().tuples.len(), 2);
        let second = coordinates(&s[1], 100).unwrap().unwrap();
        assert_eq!(second.tuples.len(), 2);
        assert_eq!(
            i32::from_le_bytes(second.tuples[1][..4].try_into().unwrap()),
            50
        );
        assert_eq!(
            r.find_field("PTAS").unwrap().data_trimmed(),
            [110, 1, 0, 0, 0, 1]
        );
        store
            .apply_records(&[
                dr(vec![
                    id("CRID", 9, 7, 3),
                    ctrl("SECC", 1, 2, 1),
                    field("SEGH", vec![4]),
                    c2(&[99]),
                ]),
                dr(vec![id("CRID", 9, 8, 3), ctrl("SECC", 2, 1, 1)]),
            ])
            .unwrap();
        assert_eq!(
            segments(&store.record(key).unwrap().fields).unwrap().len(),
            2
        );
    }
    #[test]
    fn component_deletion_only_removes_reference_and_preserves_other_records() {
        let c = |n: u32| {
            let mut d = vec![120];
            d.extend(n.to_le_bytes());
            d.push(1);
            field("CUCO", d)
        };
        let key = RecordKey { name: 125, id: 2 };
        let curve = dr(vec![
            id("CRID", 9, 1, 1),
            field("SEGH", vec![4]),
            c2(&[10, 20]),
        ]);
        let mut store = S100RecordStore::from_base(
            vec![curve, dr(vec![id("CCID", 2, 1, 1), c(9), c(9)])],
            UpdateLimits::default(),
        )
        .unwrap();
        store
            .apply_records(&[dr(vec![id("CCID", 2, 2, 3), ctrl("CCOC", 2, 1, 1)])])
            .unwrap();
        assert_eq!(
            components(&store.record(key).unwrap().fields, 100)
                .unwrap()
                .len(),
            1
        );
        assert!(store.record(RecordKey { name: 120, id: 9 }).is_some());
    }
    #[test]
    fn failed_late_operation_rolls_back_all_record_and_byte_changes() {
        let key = RecordKey { name: 115, id: 1 };
        let base = dr(vec![id("MRID", 1, 1, 1), c3(7, &[1, 2])]);
        let original = base
            .fields
            .iter()
            .map(|f| f.data.clone())
            .collect::<Vec<_>>();
        let mut store = S100RecordStore::from_base(vec![base], UpdateLimits::default()).unwrap();
        let size = store.payload_bytes();
        let good = dr(vec![
            id("MRID", 1, 2, 3),
            ctrl("COCC", 1, 1, 1),
            c3(7, &[99]),
        ]);
        for invalid in [
            dr(vec![id("MRID", 1, 4, 3), ctrl("COCC", 2, 1, 1)]),
            dr(vec![
                id("MRID", 1, 3, 3),
                ctrl("COCC", 2, 1, 1),
                c3(7, &[10]),
            ]),
            dr(vec![
                id("MRID", 1, 3, 3),
                ctrl("COCC", 3, 1, 1),
                c3(8, &[10]),
            ]),
            dr(vec![
                id("MRID", 1, 3, 3),
                ctrl("COCC", 3, 1, 1),
                field("INAS", vec![]),
            ]),
        ] {
            assert!(store.apply_records(&[good.clone(), invalid]).is_err());
            assert_eq!(store.payload_bytes(), size);
            assert_eq!(
                store
                    .record(key)
                    .unwrap()
                    .fields
                    .iter()
                    .map(|f| f.data.clone())
                    .collect::<Vec<_>>(),
                original
            );
        }
    }
    #[test]
    fn retained_deleted_identity_budget_and_identifier_reuse_are_bounded() {
        let mut store = S100RecordStore::from_base(
            vec![dr(vec![id("PRID", 1, 1, 1), field("C2IT", vec![0; 8])])],
            UpdateLimits {
                max_records: 1,
                ..UpdateLimits::default()
            },
        )
        .unwrap();
        store
            .apply_records(&[dr(vec![id("PRID", 1, 2, 2)])])
            .unwrap();
        assert!(store
            .apply_records(&[dr(vec![id("PRID", 1, 1, 1), field("C2IT", vec![0; 8])])])
            .is_err());
        assert!(store
            .apply_records(&[dr(vec![id("PRID", 2, 1, 1), field("C2IT", vec![0; 8])])])
            .is_err());
        assert!(store.record(RecordKey { name: 110, id: 2 }).is_none());
    }
    #[test]
    fn deleted_information_is_detected_even_in_unexposed_spatial_inas() {
        let mut ir = vec![150, 9, 0, 0, 0, 1, 0, 1, 0, 1];
        let mut store = S100RecordStore::from_base(
            vec![
                dr(vec![field("IRID", ir.clone())]),
                dr(vec![
                    id("PRID", 1, 1, 1),
                    field("C2IT", vec![0; 8]),
                    field("INAS", vec![150, 9, 0, 0, 0, 1, 0, 1, 0, 1]),
                ]),
            ],
            UpdateLimits::default(),
        )
        .unwrap();
        store.validate_references().unwrap();
        ir[7] = 2;
        ir[9] = 2;
        store.apply_records(&[dr(vec![field("IRID", ir)])]).unwrap();
        assert!(store.validate_references().is_err());
    }
}
