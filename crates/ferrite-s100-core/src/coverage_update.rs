//! S-101 2.0 section 4.5.2: an update cannot move a base DataCoverage limit.
//! Compare original integer geometry, not record versions, floating point
//! reconstructions, bounding boxes, or the union of unrelated coverage features.
use crate::updates::{RecordKey, S100RecordStore, UpdateRecordHeader};
use crate::{Result, S100Error};
use ferrite_iso8211::{RawField, DR};
use std::collections::{BTreeMap, BTreeSet};

type Point = [i32; 2]; // encoded latitude, longitude; chain transforms are fixed
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_POINTS: usize = 65_536;
const MAX_STEPS: usize = 1_000_000;
const MAX_DEPTH: usize = 64;
fn bad(s: &str) -> S100Error {
    S100Error::InvalidRecord(format!("S-101 DataCoverage update: {s}"))
}
#[derive(Default)]
struct Budget {
    bytes: usize,
    steps: usize,
}
impl Budget {
    fn charge(&mut self, n: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(n)
            .ok_or_else(|| bad("size overflow"))?;
        if self.bytes > MAX_BYTES {
            return Err(bad("geometry verification byte budget exceeded"));
        }
        Ok(())
    }
    fn step(&mut self) -> Result<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(bad("geometry expansion budget exceeded"));
        }
        Ok(())
    }
}
fn reserve<T>(v: &mut Vec<T>, needed: usize, budget: &mut Budget) -> Result<()> {
    if needed > MAX_POINTS {
        return Err(bad("geometry list budget exceeded"));
    }
    if needed > v.capacity() {
        let n = needed.max(v.capacity().saturating_mul(2)).min(MAX_POINTS);
        let old = v.capacity();
        budget.charge(
            (n - old)
                .checked_mul(std::mem::size_of::<T>())
                .ok_or_else(|| bad("size overflow"))?,
        )?;
        v.try_reserve_exact(n - v.len())
            .map_err(|_| bad("geometry reservation failed"))?;
        if v.capacity() > n {
            budget.charge((v.capacity() - n) * std::mem::size_of::<T>())?;
        }
    }
    Ok(())
}
fn push<T>(v: &mut Vec<T>, x: T, budget: &mut Budget) -> Result<()> {
    reserve(v, v.len() + 1, budget)?;
    v.push(x);
    Ok(())
}
fn one<'a>(dr: &'a DR, tag: &str) -> Result<Option<&'a RawField>> {
    let mut fields = dr.fields.iter().filter(|f| f.tag == tag);
    let first = fields.next();
    if fields.next().is_some() {
        return Err(bad("duplicate singleton geometry field"));
    }
    Ok(first)
}
fn key(d: &[u8]) -> RecordKey {
    RecordKey {
        name: d[0],
        id: u32::from_le_bytes(d[1..5].try_into().unwrap()),
    }
}
fn point(d: &[u8]) -> Point {
    [
        i32::from_le_bytes(d[..4].try_into().unwrap()),
        i32::from_le_bytes(d[4..8].try_into().unwrap()),
    ]
}
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Polygon {
    exterior: Vec<Point>,
    holes: Vec<Vec<Point>>,
}
#[derive(Default)]
struct Snapshot {
    features: BTreeMap<[u8; 8], Vec<Polygon>>,
    dependencies: BTreeSet<RecordKey>,
    coverage_records: BTreeSet<RecordKey>,
    #[cfg(test)]
    feature_record_visits: usize,
}
/// A guard is local to the retained raw chain; it is neither an authentication
/// token nor a persisted memo. A base must contain DataCoverage before updates
/// are applied (S-101 4.5.3); a later insertion cannot repair an invalid base.
pub(crate) struct CoverageUpdateGuard {
    base: Snapshot,
    code: Option<u16>,
    #[cfg(test)]
    last_feature_record_visits: usize,
}
impl CoverageUpdateGuard {
    pub(crate) fn capture(store: &S100RecordStore, code: Option<u16>) -> Result<Self> {
        if code.is_none() {
            return Err(bad("base dictionary has no DataCoverage type (4.5.3)"));
        }
        let base = snapshot(store, code, None)?;
        if base.features.is_empty() {
            return Err(bad("base dataset has no DataCoverage feature (4.5.3)"));
        }
        Ok(Self {
            base,
            code,
            #[cfg(test)]
            last_feature_record_visits: 0,
        })
    }
    pub(crate) fn verify_after(&mut self, store: &S100RecordStore, updates: &[DR]) -> Result<()> {
        if self.base.features.is_empty() {
            return Ok(());
        }
        let mut relevant = false;
        for dr in updates {
            let h = UpdateRecordHeader::parse(
                dr.fields
                    .first()
                    .ok_or_else(|| bad("empty update record"))?,
            )?
            .ok_or_else(|| bad("update without record identity"))?;
            relevant |= h.key.name == 100 || self.base.dependencies.contains(&h.key);
        }
        if !relevant {
            return Ok(());
        }
        // Query only retained base coverage record identities and feature
        // identities touched by this update. An equivalent delete/reinsert can
        // change RCID while retaining FOID, so the incoming keys matter too.
        // No O(all dataset records) rescan for each update.
        let mut candidates = BTreeSet::new();
        let mut budget = Budget::default();
        for &k in &self.base.coverage_records {
            budget.charge(256)?;
            candidates.insert(k);
        }
        for dr in updates {
            let h = UpdateRecordHeader::parse(&dr.fields[0])?.unwrap();
            if h.key.name == 100 && !candidates.contains(&h.key) {
                budget.charge(256)?;
                candidates.insert(h.key);
            }
        }
        let current = snapshot_records(
            store,
            self.code,
            Some(&self.base.features),
            candidates
                .iter()
                .filter_map(|k| store.record(*k).map(|r| (*k, r))),
            budget,
        )?;
        #[cfg(test)]
        {
            self.last_feature_record_visits = current.feature_record_visits;
        }
        if self.base.features != current.features {
            return Err(bad(
                "base feature limit differs or exact equivalence is unverified; a changed limit requires a New Edition (4.5.2)",
            ));
        }
        // Equivalent replacement spatial records become dependencies for the
        // NEXT update too. Never retain an old dependency permission after a
        // successful record remapping.
        self.base.dependencies = current.dependencies;
        self.base.coverage_records = current.coverage_records;
        Ok(())
    }
}
fn snapshot(
    store: &S100RecordStore,
    code: Option<u16>,
    selected: Option<&BTreeMap<[u8; 8], Vec<Polygon>>>,
) -> Result<Snapshot> {
    snapshot_records(store, code, selected, store.records(), Budget::default())
}
fn snapshot_records<'a>(
    store: &'a S100RecordStore,
    code: Option<u16>,
    selected: Option<&BTreeMap<[u8; 8], Vec<Polygon>>>,
    records: impl Iterator<Item = (RecordKey, &'a DR)>,
    mut budget: Budget,
) -> Result<Snapshot> {
    let mut out = Snapshot::default();
    let Some(code) = code else {
        return Ok(out);
    };
    for (identity, dr) in records {
        #[cfg(test)]
        {
            out.feature_record_visits += 1;
        }
        if identity.name != 100 {
            continue;
        }
        let d = dr.fields[0].data_trimmed();
        if u16::from_le_bytes(d[5..7].try_into().unwrap()) != code {
            continue;
        }
        let foid = one(dr, "FOID")?
            .ok_or_else(|| bad("coverage has no FOID"))?
            .data_trimmed();
        let foid: [u8; 8] = foid.try_into().map_err(|_| bad("invalid coverage FOID"))?;
        if selected.is_some_and(|s| !s.contains_key(&foid)) {
            continue;
        }
        budget.charge(256)?;
        out.coverage_records.insert(identity);
        budget.charge(256)?;
        if !out.dependencies.insert(identity) {
            return Err(bad("duplicate coverage identity"));
        }
        let mut walker = Walker {
            store,
            dependencies: &mut out.dependencies,
            budget: &mut budget,
            active: BTreeSet::new(),
        };
        let mut polygons = Vec::new();
        for f in dr.fields.iter().filter(|f| f.tag == "SPAS") {
            let d = f.data_trimmed();
            crate::cell::validate_materialized_spas(d)?;
            for t in d.as_chunks::<15>().0 {
                if t[0] != 130 {
                    return Err(bad("coverage association is not a surface"));
                }
                let polygon = walker.surface(key(t))?;
                push(&mut polygons, polygon, walker.budget)?;
            }
        }
        if polygons.is_empty() {
            return Err(bad("coverage has no surface"));
        }
        polygons.sort();
        budget.charge(256)?;
        if out.features.insert(foid, polygons).is_some() {
            return Err(bad("duplicate coverage FOID"));
        }
    }
    Ok(out)
}
struct Walker<'a, 'b> {
    store: &'a S100RecordStore,
    dependencies: &'b mut BTreeSet<RecordKey>,
    budget: &'b mut Budget,
    active: BTreeSet<RecordKey>,
}
impl<'a> Walker<'a, '_> {
    fn record(&mut self, k: RecordKey) -> Result<&'a DR> {
        self.budget.step()?;
        if self.dependencies.insert(k) {
            self.budget.charge(256)?;
        }
        self.store
            .record(k)
            .ok_or_else(|| bad("missing boundary dependency"))
    }
    fn endpoint(&mut self, k: RecordKey) -> Result<Point> {
        if k.name != 110 {
            return Err(bad("invalid point endpoint type"));
        }
        let dr = self.record(k)?;
        // A 2-D boundary may share a topological point with a 3-D feature.
        // Validate the same alternate coordinate tuple as the cell parser,
        // then compare ONLY its original horizontal integers. Z is not a limit.
        let mut coordinates = dr
            .fields
            .iter()
            .filter(|f| matches!(f.tag.as_str(), "C2IT" | "C3IT"));
        let field = coordinates
            .next()
            .ok_or_else(|| bad("boundary endpoint has no coordinates"))?;
        if coordinates.next().is_some() {
            return Err(bad("boundary endpoint has multiple coordinate tuples"));
        }
        let d = field.data_trimmed();
        match (field.tag.as_str(), d.len()) {
            ("C2IT", 8) => Ok(point(d)),
            ("C3IT", 13) => Ok(point(&d[1..9])),
            _ => Err(bad("malformed boundary endpoint")),
        }
    }
    fn curve(&mut self, k: RecordKey, forward: bool, depth: usize) -> Result<Vec<Point>> {
        if depth > MAX_DEPTH || !self.active.insert(k) {
            return Err(bad("composite cycle or depth budget exceeded"));
        }
        let result = self.curve_inner(k, forward, depth);
        self.active.remove(&k);
        result
    }
    fn curve_inner(&mut self, k: RecordKey, forward: bool, depth: usize) -> Result<Vec<Point>> {
        let dr = self.record(k)?;
        let mut out = Vec::new();
        match k.name {
            120 => {
                let mut new_segment = false;
                for f in &dr.fields {
                    match f.tag.as_str() {
                        "SEGH" => {
                            if f.data_trimmed() != [4] {
                                return Err(bad("non-loxodromic segment"));
                            }
                            new_segment = true;
                        }
                        "C2IL" => {
                            let d = f.data_trimmed();
                            if !d.len().is_multiple_of(8) {
                                return Err(bad("truncated coordinate list"));
                            }
                            for t in d.as_chunks::<8>().0 {
                                self.budget.step()?;
                                let p = point(t);
                                if new_segment && !out.is_empty() && out.last() != Some(&p) {
                                    return Err(bad("disconnected curve segments"));
                                }
                                new_segment = false;
                                if out.last() != Some(&p) {
                                    push(&mut out, p, self.budget)?;
                                }
                            }
                        }
                        "C3IL" => return Err(bad("unsupported non-2-D coverage boundary")),
                        _ => {}
                    }
                }
                if out.len() < 2 {
                    return Err(bad("empty boundary curve"));
                }
                if let Some(ptas) = one(dr, "PTAS")? {
                    let d = ptas.data_trimmed();
                    if !d.len().is_multiple_of(6) {
                        return Err(bad("truncated PTAS"));
                    }
                    let (mut start, mut end) = (None, None);
                    for t in d.as_chunks::<6>().0 {
                        if !matches!(t[5], 1..=3) {
                            return Err(bad("invalid PTAS topology"));
                        }
                        let p = self.endpoint(key(t))?;
                        if matches!(t[5], 1 | 3) && start.replace(p).is_some() {
                            return Err(bad("duplicate start endpoint"));
                        }
                        if matches!(t[5], 2 | 3) && end.replace(p).is_some() {
                            return Err(bad("duplicate end endpoint"));
                        }
                    }
                    if start.as_ref() != out.first() || end.as_ref() != out.last() {
                        return Err(bad("PTAS does not match boundary coordinates"));
                    }
                }
                if !forward {
                    out.reverse();
                }
            }
            125 => {
                // Reversing a composite reverses both component order and each
                // component's direction. Fields retain their original order.
                for i in 0..dr.fields.len() {
                    let f = &dr.fields[if forward { i } else { dr.fields.len() - 1 - i }];
                    if f.tag != "CUCO" {
                        continue;
                    }
                    let d = f.data_trimmed();
                    crate::cell::validate_curve_association_field(d, false)?;
                    let n = d.len() / 6;
                    for j in 0..n {
                        let i = if forward { j } else { n - 1 - j };
                        let t = &d[i * 6..i * 6 + 6];
                        let child = self.curve(key(t), (t[5] == 1) == forward, depth + 1)?;
                        append(&mut out, &child, self.budget)?;
                    }
                }
                if out.len() < 2 {
                    return Err(bad("empty composite boundary"));
                }
            }
            _ => return Err(bad("invalid boundary curve type")),
        }
        Ok(out)
    }
    fn surface(&mut self, k: RecordKey) -> Result<Polygon> {
        let dr = self.record(k)?;
        let (mut exterior, mut interior) = (Vec::new(), Vec::new());
        for f in dr.fields.iter().filter(|f| f.tag == "RIAS") {
            let d = f.data_trimmed();
            crate::cell::validate_curve_association_field(d, true)?;
            for t in d.as_chunks::<8>().0 {
                let path = self.curve(key(t), t[5] == 1, 0)?;
                push(
                    if t[6] == 1 {
                        &mut exterior
                    } else {
                        &mut interior
                    },
                    path,
                    self.budget,
                )?;
            }
        }
        let mut rings = assemble(exterior, self.budget)?;
        if rings.len() != 1 {
            return Err(bad("surface must have one exterior ring"));
        }
        let mut holes = assemble(interior, self.budget)?;
        holes.sort();
        Ok(Polygon {
            exterior: rings.pop().unwrap(),
            holes,
        })
    }
}
fn append(out: &mut Vec<Point>, next: &[Point], budget: &mut Budget) -> Result<()> {
    if !out.is_empty() && out.last() != next.first() {
        return Err(bad("disconnected oriented boundary"));
    }
    let next = &next[usize::from(!out.is_empty())..];
    reserve(out, out.len() + next.len(), budget)?;
    out.extend_from_slice(next);
    Ok(())
}
fn assemble(mut paths: Vec<Vec<Point>>, budget: &mut Budget) -> Result<Vec<Vec<Point>>> {
    let mut starts = BTreeMap::new();
    for (i, p) in paths.iter().enumerate() {
        if p.len() < 2 {
            return Err(bad("empty ring component"));
        }
        budget.charge(256)?;
        if starts.insert(p[0], i).is_some() {
            return Err(bad("ambiguous ring component junction"));
        }
    }
    let mut rings = Vec::new();
    while let Some((&start, &i)) = starts.first_key_value() {
        starts.remove(&start);
        let mut ring = std::mem::take(&mut paths[i]);
        while ring.last() != Some(&start) {
            budget.step()?;
            let end = *ring.last().unwrap();
            let next = starts
                .remove(&end)
                .ok_or_else(|| bad("unclosed boundary ring"))?;
            append(&mut ring, &std::mem::take(&mut paths[next]), budget)?;
        }
        canonicalize(&mut ring, budget)?;
        push(&mut rings, ring, budget)?;
    }
    rings.sort();
    Ok(rings)
}
// Only exact meridian/parallel subdivision is removed. General collinearity in
// longitude/latitude is NOT collinearity of loxodromic segments. No tolerance.
fn redundant(a: Point, b: Point, c: Point) -> bool {
    (a[0] == b[0]
        && b[0] == c[0]
        && (i64::from(a[1]) - i64::from(c[1])).abs() <= 1_800_000_000
        && (a[1].min(c[1])..=a[1].max(c[1])).contains(&b[1]))
        || (a[1] == b[1] && b[1] == c[1] && (a[0].min(c[0])..=a[0].max(c[0])).contains(&b[0]))
}
fn canonicalize(p: &mut Vec<Point>, budget: &mut Budget) -> Result<()> {
    p.dedup();
    if p.len() < 4 || p.first() != p.last() {
        return Err(bad("invalid closed boundary ring"));
    }
    p.pop();
    let n = p.len();
    let corner = (0..n)
        .find(|&i| !redundant(p[(i + n - 1) % n], p[i], p[(i + 1) % n]))
        .ok_or_else(|| bad("degenerate ring"))?;
    p.rotate_left(corner);
    let mut len = 0;
    for i in 0..p.len() {
        let next = p[i];
        while len >= 2 && redundant(p[len - 2], p[len - 1], next) {
            len -= 1;
        }
        p[len] = next;
        len += 1;
    }
    p.truncate(len);
    while p.len() >= 3 && redundant(p[p.len() - 2], p[p.len() - 1], p[0]) {
        p.pop();
    }
    if p.len() < 3 {
        return Err(bad("degenerate ring"));
    }
    let mut unique = BTreeSet::new();
    for &v in p.iter() {
        budget.charge(96)?;
        if !unique.insert(v) {
            return Err(bad("repeated nonadjacent ring vertex"));
        }
    }
    let start = p.iter().enumerate().min_by_key(|(_, v)| **v).unwrap().0;
    p.rotate_left(start);
    for i in 1..p.len() {
        let ordering = p[i].cmp(&p[p.len() - i]);
        if ordering.is_lt() {
            break;
        }
        if ordering.is_gt() {
            p[1..].reverse();
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::updates::UpdateLimits;
    use ferrite_iso8211::FIELD_TERMINATOR;
    fn field(tag: &str, mut d: Vec<u8>) -> RawField {
        d.push(FIELD_TERMINATOR);
        RawField::new(tag.into(), d)
    }
    fn encoded(fields: &[RawField]) -> Vec<u8> {
        let base = 24 + fields.len() * 14 + 1;
        let length = base + fields.iter().map(|f| f.data.len()).sum::<usize>();
        let mut bytes = format!("{length:05}3D 1 00{base:05}   5504").into_bytes();
        let mut offset = 0;
        for f in fields {
            bytes.extend(format!("{}{:05}{offset:05}", f.tag, f.data.len()).as_bytes());
            offset += f.data.len();
        }
        bytes.push(FIELD_TERMINATOR);
        for f in fields {
            bytes.extend(&f.data);
        }
        bytes
    }
    fn dr(fields: Vec<RawField>) -> DR {
        DR::parse(&encoded(&fields)).unwrap()
    }
    fn id(tag: &str, name: u8, n: u32, version: u16, operation: u8) -> RawField {
        let mut d = vec![name];
        d.extend(n.to_le_bytes());
        if name == 100 {
            d.extend(1u16.to_le_bytes());
        }
        d.extend(version.to_le_bytes());
        d.push(operation);
        field(tag, d)
    }
    fn coordinates(points: &[Point]) -> RawField {
        field(
            "C2IL",
            points
                .iter()
                .flat_map(|p| p.iter().flat_map(|v| v.to_le_bytes()))
                .collect(),
        )
    }
    fn ref_tuple(name: u8, id: u32, orientation: u8) -> Vec<u8> {
        let mut d = vec![name];
        d.extend(id.to_le_bytes());
        d.push(orientation);
        d
    }
    fn curve(n: u32, p: &[Point]) -> DR {
        dr(vec![
            id("CRID", 120, n, 1, 1),
            field("SEGH", vec![4]),
            coordinates(p),
        ])
    }
    fn surface(exterior: &[(u8, u32, u8)], holes: &[(u8, u32, u8)]) -> DR {
        let mut d = Vec::new();
        for (usage, refs) in [(1, exterior), (2, holes)] {
            for &(name, id, orientation) in refs {
                d.extend(ref_tuple(name, id, orientation));
                d.extend([usage, 1]);
            }
        }
        dr(vec![id("SRID", 130, 1, 1, 1), field("RIAS", d)])
    }
    fn feature() -> DR {
        let mut d = ref_tuple(130, 1, 255);
        d.extend([255; 8]);
        d.push(1);
        dr(vec![
            id("FRID", 100, 1, 1, 1),
            field("FOID", vec![1, 0, 10, 0, 0, 0, 0, 0]),
            field("SPAS", d),
        ])
    }
    fn base(points: &[Point]) -> S100RecordStore {
        S100RecordStore::from_base(
            vec![curve(1, points), surface(&[(120, 1, 1)], &[]), feature()],
            UpdateLimits::default(),
        )
        .unwrap()
    }
    fn modified_curve(n: u32, points: &[Point], version: u16) -> DR {
        let mut count = vec![3, 1, 0];
        count.extend((points.len() as u16).to_le_bytes());
        dr(vec![
            id("CRID", 120, n, version, 3),
            field("SECC", vec![3, 1, 0, 1, 0]),
            field("SEGH", vec![4]),
            field("COCC", count),
            coordinates(points),
        ])
    }
    const RECT: [Point; 5] = [[0, 0], [0, 10], [10, 10], [10, 0], [0, 0]];
    #[test]
    fn same_bbox_coordinate_change_requires_new_edition_before_a_later_restore() {
        let mut store = base(&RECT);
        let mut guard = CoverageUpdateGuard::capture(&store, Some(1)).unwrap();
        let mut changed = RECT;
        changed[2] = [9, 9];
        let first = modified_curve(1, &changed, 2);
        store.apply_records(std::slice::from_ref(&first)).unwrap();
        let error = guard
            .verify_after(&store, std::slice::from_ref(&first))
            .unwrap_err()
            .to_string();
        assert!(error.contains("New Edition"));
        let second = modified_curve(1, &RECT, 3);
        store.apply_records(std::slice::from_ref(&second)).unwrap();
        // The unchanged FINAL geometry does not excuse the rejected first step.
        guard
            .verify_after(&store, std::slice::from_ref(&second))
            .unwrap();
    }
    #[test]
    fn version_only_update_and_ring_rotation_reversal_keep_the_limit() {
        let mut store = base(&RECT);
        let mut guard = CoverageUpdateGuard::capture(&store, Some(1)).unwrap();
        let version = dr(vec![id("CRID", 120, 1, 2, 3)]);
        store.apply_records(std::slice::from_ref(&version)).unwrap();
        guard
            .verify_after(&store, std::slice::from_ref(&version))
            .unwrap();
        let reversed = [[10, 10], [0, 10], [0, 0], [10, 0], [10, 10]];
        let next = modified_curve(1, &reversed, 3);
        store.apply_records(std::slice::from_ref(&next)).unwrap();
        guard
            .verify_after(&store, std::slice::from_ref(&next))
            .unwrap();
    }
    #[test]
    fn subsequent_updates_visit_only_coverage_and_touched_feature_records() {
        let mut records = vec![curve(1, &RECT), surface(&[(120, 1, 1)], &[]), feature()];
        for n in 1000..11000 {
            records.push(dr(vec![
                id("PRID", 110, n, 1, 1),
                field("C2IT", vec![0; 8]),
            ]));
        }
        let mut store = S100RecordStore::from_base(records, UpdateLimits::default()).unwrap();
        let mut guard = CoverageUpdateGuard::capture(&store, Some(1)).unwrap();
        let update = dr(vec![id("CRID", 120, 1, 2, 3)]);
        store.apply_records(std::slice::from_ref(&update)).unwrap();
        guard
            .verify_after(&store, std::slice::from_ref(&update))
            .unwrap();
        assert_eq!(guard.last_feature_record_visits, 1);
    }
    #[test]
    fn deleted_coverage_is_not_a_valid_empty_limit() {
        let mut store = base(&RECT);
        let mut guard = CoverageUpdateGuard::capture(&store, Some(1)).unwrap();
        let delete = dr(vec![id("FRID", 100, 1, 2, 2)]);
        store.apply_records(std::slice::from_ref(&delete)).unwrap();
        assert!(guard
            .verify_after(&store, std::slice::from_ref(&delete))
            .is_err());
    }
    #[test]
    fn equivalent_record_remapping_tracks_new_geometry_dependencies() {
        let mut store = base(&RECT);
        let mut guard = CoverageUpdateGuard::capture(&store, Some(1)).unwrap();
        let mut remove = ref_tuple(130, 1, 255);
        remove.extend([255; 8]);
        remove.push(2);
        let mut insert = ref_tuple(130, 2, 255);
        insert.extend([255; 8]);
        insert.push(1);
        remove.extend(insert);
        let replacement_surface = dr(vec![
            id("SRID", 130, 2, 1, 1),
            field("RIAS", vec![120, 2, 0, 0, 0, 1, 1, 1]),
        ]);
        let update = vec![
            curve(2, &RECT),
            replacement_surface,
            dr(vec![id("FRID", 100, 1, 2, 3), field("SPAS", remove)]),
        ];
        store.apply_records(&update).unwrap();
        guard.verify_after(&store, &update).unwrap();
        let mut changed = RECT;
        changed[2] = [9, 9];
        let next = modified_curve(2, &changed, 2);
        store.apply_records(std::slice::from_ref(&next)).unwrap();
        assert!(guard
            .verify_after(&store, std::slice::from_ref(&next))
            .is_err());
    }
    #[test]
    fn nested_reverse_composite_and_unordered_surface_components_are_exact() {
        let mut original = base(&RECT);
        let baseline = CoverageUpdateGuard::capture(&original, Some(1)).unwrap();
        let a = curve(2, &RECT[..3]);
        let b = curve(3, &[RECT[2], RECT[3], RECT[4]]);
        let mut components = ref_tuple(120, 3, 2);
        components.extend(ref_tuple(120, 2, 2));
        let inner = dr(vec![id("CCID", 125, 4, 1, 1), field("CUCO", components)]);
        let outer = dr(vec![
            id("CCID", 125, 5, 1, 1),
            field("CUCO", ref_tuple(125, 4, 2)),
        ]);
        let composite = S100RecordStore::from_base(
            vec![
                a.clone(),
                b.clone(),
                inner,
                outer,
                surface(&[(125, 5, 1)], &[]),
                feature(),
            ],
            UpdateLimits::default(),
        )
        .unwrap();
        assert_eq!(
            baseline.base.features,
            snapshot(&composite, Some(1), None).unwrap().features
        );
        let unordered = S100RecordStore::from_base(
            vec![a, b, surface(&[(120, 3, 1), (120, 2, 1)], &[]), feature()],
            UpdateLimits::default(),
        )
        .unwrap();
        assert_eq!(
            baseline.base.features,
            snapshot(&unordered, Some(1), None).unwrap().features
        );
        // An unrelated spatial modification need not expand the base geometry.
        original.apply_records(&[curve(9, &RECT)]).unwrap();
    }
    #[test]
    fn interior_ring_change_is_detected_even_with_identical_exterior() {
        let hole = [[2, 2], [2, 4], [4, 4], [4, 2], [2, 2]];
        let mut store = S100RecordStore::from_base(
            vec![
                curve(1, &RECT),
                curve(2, &hole),
                surface(&[(120, 1, 1)], &[(120, 2, 1)]),
                feature(),
            ],
            UpdateLimits::default(),
        )
        .unwrap();
        let mut guard = CoverageUpdateGuard::capture(&store, Some(1)).unwrap();
        let mut changed = hole;
        changed[2] = [3, 3];
        let update = modified_curve(2, &changed, 2);
        store.apply_records(std::slice::from_ref(&update)).unwrap();
        assert!(guard
            .verify_after(&store, std::slice::from_ref(&update))
            .is_err());
    }
    #[test]
    fn swapping_limits_between_base_foids_is_rejected_even_when_the_dataset_union_is_identical() {
        let shifted = RECT.map(|p| [p[0] + 100, p[1] + 100]);
        let mut second_surface = surface(&[(120, 2, 1)], &[]);
        second_surface.fields[0].data[1..5].copy_from_slice(&2u32.to_le_bytes());
        let mut second_feature = feature();
        second_feature.fields[0].data[1..5].copy_from_slice(&2u32.to_le_bytes());
        second_feature.fields[1].data[2..6].copy_from_slice(&11u32.to_le_bytes());
        second_feature.fields[2].data[1..5].copy_from_slice(&2u32.to_le_bytes());
        let mut store = S100RecordStore::from_base(
            vec![
                curve(1, &RECT),
                curve(2, &shifted),
                surface(&[(120, 1, 1)], &[]),
                second_surface,
                feature(),
                second_feature,
            ],
            UpdateLimits::default(),
        )
        .unwrap();
        let mut guard = CoverageUpdateGuard::capture(&store, Some(1)).unwrap();
        let replacement = |feature_id: u32, from: u32, to: u32| {
            let mut remove = ref_tuple(130, from, 255);
            remove.extend([255; 8]);
            remove.push(2);
            let mut insert = ref_tuple(130, to, 255);
            insert.extend([255; 8]);
            insert.push(1);
            remove.extend(insert);
            dr(vec![
                id("FRID", 100, feature_id, 2, 3),
                field("SPAS", remove),
            ])
        };
        let updates = vec![replacement(1, 1, 2), replacement(2, 2, 1)];
        store.apply_records(&updates).unwrap();
        let current = snapshot(&store, Some(1), None).unwrap();
        let mut original_union: Vec<_> = guard.base.features.values().flatten().collect();
        original_union.sort();
        let mut current_union: Vec<_> = current.features.values().flatten().collect();
        current_union.sort();
        assert_eq!(original_union, current_union);
        assert!(guard.verify_after(&store, &updates).is_err());
    }
    #[test]
    fn interior_ring_order_is_not_a_change_of_the_limit() {
        let first = [[2, 2], [2, 4], [4, 4], [4, 2], [2, 2]];
        let second = first.map(|p| [p[0] + 4, p[1] + 4]);
        let create = |reverse: bool| {
            S100RecordStore::from_base(
                vec![
                    curve(1, &RECT),
                    curve(2, &first),
                    curve(3, &second),
                    surface(
                        &[(120, 1, 1)],
                        if reverse {
                            &[(120, 3, 1), (120, 2, 1)]
                        } else {
                            &[(120, 2, 1), (120, 3, 1)]
                        },
                    ),
                    feature(),
                ],
                UpdateLimits::default(),
            )
            .unwrap()
        };
        assert_eq!(
            snapshot(&create(false), Some(1), None).unwrap().features,
            snapshot(&create(true), Some(1), None).unwrap().features
        );
    }
    #[test]
    fn exact_axis_subdivision_is_removed_but_latlon_collinearity_is_not() {
        let mut p = vec![
            [0, 0],
            [0, 5],
            [0, 10],
            [5, 10],
            [10, 10],
            [10, 5],
            [10, 0],
            [5, 0],
            [0, 0],
        ];
        let mut q = RECT.to_vec();
        canonicalize(&mut p, &mut Budget::default()).unwrap();
        canonicalize(&mut q, &mut Budget::default()).unwrap();
        assert_eq!(p, q);
        assert!(!redundant([0, 0], [5, 5], [10, 10]));
        assert!(!redundant([0, 0], [0, 20], [0, 10]));
        // Never collapse a wide parallel across a longitude-wrap ambiguity.
        assert!(!redundant([0, -1_700_000_000], [0, 0], [0, 1_700_000_000]));
    }
    #[test]
    fn shared_three_dimensional_endpoint_keeps_xy_and_vertical_only_update_is_allowed() {
        let mut c = curve(1, &RECT);
        c.fields.push(field("PTAS", ref_tuple(110, 1, 3)));
        let tuple = |z: i32| {
            let mut d = vec![1];
            for v in [0i32, 0, z] {
                d.extend(v.to_le_bytes());
            }
            d
        };
        let pt = dr(vec![id("PRID", 110, 1, 1, 1), field("C3IT", tuple(123))]);
        let mut store = S100RecordStore::from_base(
            vec![c, pt, surface(&[(120, 1, 1)], &[]), feature()],
            UpdateLimits::default(),
        )
        .unwrap();
        let mut guard = CoverageUpdateGuard::capture(&store, Some(1)).unwrap();
        let update = dr(vec![id("PRID", 110, 1, 2, 3), field("C3IT", tuple(456))]);
        store.apply_records(std::slice::from_ref(&update)).unwrap();
        guard
            .verify_after(&store, std::slice::from_ref(&update))
            .unwrap();
        let mut changed = tuple(456);
        changed[1..5].copy_from_slice(&1i32.to_le_bytes());
        let update = dr(vec![id("PRID", 110, 1, 3, 3), field("C3IT", changed)]);
        store.apply_records(std::slice::from_ref(&update)).unwrap();
        assert!(guard
            .verify_after(&store, std::slice::from_ref(&update))
            .is_err());
    }
    #[test]
    fn cycles_ptas_mismatch_and_resource_limits_fail_without_geometry_permission() {
        let cyclic = dr(vec![
            id("CCID", 125, 2, 1, 1),
            field("CUCO", ref_tuple(125, 2, 1)),
        ]);
        let store = S100RecordStore::from_base(
            vec![cyclic, surface(&[(125, 2, 1)], &[]), feature()],
            UpdateLimits::default(),
        )
        .unwrap();
        assert!(CoverageUpdateGuard::capture(&store, Some(1)).is_err());
        let mut c = curve(1, &RECT);
        let mut endpoint = ref_tuple(110, 1, 3);
        c.fields.push(field("PTAS", std::mem::take(&mut endpoint)));
        let pt = dr(vec![
            id("PRID", 110, 1, 1, 1),
            field("C2IT", [1i32.to_le_bytes(), 0i32.to_le_bytes()].concat()),
        ]);
        let store = S100RecordStore::from_base(
            vec![c, pt, surface(&[(120, 1, 1)], &[]), feature()],
            UpdateLimits::default(),
        )
        .unwrap();
        assert!(CoverageUpdateGuard::capture(&store, Some(1)).is_err());
        assert!(Budget::default().charge(MAX_BYTES + 1).is_err());
        let mut budget = Budget {
            steps: MAX_STEPS,
            ..Budget::default()
        };
        assert!(budget.step().is_err());
        let mut v = Vec::<Point>::new();
        assert!(reserve(&mut v, MAX_POINTS + 1, &mut Budget::default()).is_err());
        assert!(v.is_empty());
    }
    fn dataset(profile: u8, number: u16, records: &[DR]) -> Vec<u8> {
        let mut dsid = vec![10, 1, 0, 0, 0];
        let edition = if profile == 1 {
            "3".to_owned()
        } else {
            format!("3.{number}")
        };
        dsid.extend(format!("S-100 Part 10a\x1f5.2\x1fINT.IHO.S-101.2.0\x1f2.0\x1f{profile}\x1f101AA00TEST.{number:03}\x1fBoundary fixture\x1f20241016EN\x1f\x1f{edition}\x1f").as_bytes());
        let mut dssi = vec![0; 64];
        dssi[24..28].copy_from_slice(&10_000_000u32.to_le_bytes());
        dssi[28..32].copy_from_slice(&10_000_000u32.to_le_bytes());
        dssi[32..36].copy_from_slice(&10u32.to_le_bytes());
        let mut fields = vec![field("DSID", dsid), field("DSSI", dssi)];
        if profile == 1 {
            fields.push(field("FTCS", b"DataCoverage\x1f\x01\x00".to_vec()));
        }
        let mut out = b"000253LE1 0000025 ! 1104\x1e".to_vec();
        out.extend(encoded(&fields));
        for r in records {
            out.extend(encoded(&r.fields));
        }
        out
    }
    pub(crate) fn identity_base_dataset() -> Vec<u8> {
        dataset(
            1,
            0,
            &[curve(1, &RECT), surface(&[(120, 1, 1)], &[]), feature()],
        )
    }
    #[test]
    fn missing_base_coverage_cannot_be_repaired_by_a_future_update() {
        let no_features =
            S100RecordStore::from_base(vec![curve(1, &RECT)], UpdateLimits::default()).unwrap();
        assert!(CoverageUpdateGuard::capture(&no_features, Some(1)).is_err());
        assert!(CoverageUpdateGuard::capture(&base(&RECT), None).is_err());
    }
    #[test]
    fn production_chain_rejects_temporary_boundary_change_before_restore() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ferrite-coverage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let base = dir.join("cell.000");
        let first = dir.join("cell.001");
        let second = dir.join("cell.002");
        std::fs::write(
            &base,
            dataset(
                1,
                0,
                &[curve(1, &RECT), surface(&[(120, 1, 1)], &[]), feature()],
            ),
        )
        .unwrap();
        let mut changed = RECT;
        changed[2] = [9, 9];
        std::fs::write(&first, dataset(2, 1, &[modified_curve(1, &changed, 2)])).unwrap();
        std::fs::write(&second, dataset(2, 2, &[modified_curve(1, &RECT, 3)])).unwrap();
        let e = crate::S101Cell::load_update_chain_from_with_identity(
            &base,
            &base,
            &[first.clone(), second.clone()],
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("New Edition"), "{e}");
        std::fs::write(&first, dataset(2, 1, &[modified_curve(1, &RECT, 2)])).unwrap();
        let (cell, _) =
            crate::S101Cell::load_update_chain_from_with_identity(&base, &base, &[first, second])
                .unwrap();
        assert_eq!(cell.dsid.update_number, 2);
    }
}
