//! Product metadata admission, not removal permission. Catalogue authentication,
//! retained original signature equality, producer authority, replacement staging
//! and durable replay history remain separate gates.
use anyhow::{bail, ensure, Context, Result};
use chrono::{NaiveDate, NaiveTime};
use ferrite_security::{IncomingAsciiResourceAbsence, OriginalDatasetAuthentication};
use roxmltree::Node;
use std::collections::BTreeMap;
const XC: &str = "http://www.iho.int/s100/xc/5.2";
const GML: &str = "http://www.opengis.net/gml/3.2";
const GEX: &str = "http://standards.iso.org/iso/19115/-3/gex/1.0";
const GCO: &str = "http://standards.iso.org/iso/19115/-3/gco/1.0";
const MCO: &str = "http://standards.iso.org/iso/19115/-3/mco/1.0";
const CIT: &str = "http://standards.iso.org/iso/19115/-3/cit/2.0";
#[derive(Debug, Clone, PartialEq)]
struct Metadata {
    uri: String,
    producer: String,
    edition: u32,
    date: NaiveDate,
    time: Option<String>,
    purpose: u8,
    replaced: Option<bool>,
    replacements: Vec<String>,
    mandatory: BTreeMap<String, String>,
}
/// Constructed only from a physically authenticated original dataset proof.
/// This does not prove HDF5 consistency with the discovery polygon/bounds.
#[derive(Debug)]
pub struct AuthenticatedS102OriginalMetadata {
    metadata: Metadata,
    resource_sha384: String,
    catalogue_sha384: String,
}
impl AuthenticatedS102OriginalMetadata {
    pub fn from_authentication(original: &OriginalDatasetAuthentication) -> Result<Self> {
        let view = original.original_entry_view()?;
        let metadata = parse(view.entry())?;
        ensure!(
            matches!(metadata.purpose, 1 | 2),
            "Original S-102 target must be a new dataset or new edition"
        );
        Ok(Self {
            metadata,
            resource_sha384: original.resource_sha384().into(),
            catalogue_sha384: original.catalogue_sha384().into(),
        })
    }
    pub fn resource_uri(&self) -> &str {
        &self.metadata.uri
    }
    pub fn producer_code(&self) -> &str {
        &self.metadata.producer
    }
    pub fn resource_sha384(&self) -> &str {
        &self.resource_sha384
    }
    pub fn catalogue_sha384(&self) -> &str {
        &self.catalogue_sha384
    }
    /// Original dataset date, distinct from the fileless cancellation notice date.
    pub fn issue_date(&self) -> NaiveDate {
        self.metadata.date
    }
}
/// Authenticated metadata for an absent incoming resource. Borrowed absence
/// capability pins the exact owned incoming catalogue/tree. No data digest or
/// VerifiedResource is manufactured for the missing payload.
pub struct S102CancellationMetadata<'a> {
    metadata: Metadata,
    absence: IncomingAsciiResourceAbsence<'a>,
}
impl<'a> S102CancellationMetadata<'a> {
    pub fn from_absence(absence: IncomingAsciiResourceAbsence<'a>) -> Result<Self> {
        let metadata = {
            let view = absence.incoming_exchange().catalogue().discovery_view()?;
            let mut matches = view.dataset_entries().filter(|n| {
                one(*n, XC, "fileName")
                    .and_then(value)
                    .is_ok_and(|v| v == format!("file:/{}", absence.relative_name()))
            });
            let entry = matches
                .next()
                .context("Cancellation URI has no direct authenticated discovery entry")?;
            ensure!(matches.next().is_none(), "Ambiguous cancellation entry");
            parse(entry)?
        };
        ensure!(
            metadata.purpose == 5,
            "Only S-102 cancellation may use incoming absence"
        );
        Ok(Self { metadata, absence })
    }
    pub fn resource_uri(&self) -> &str {
        &self.metadata.uri
    }
    pub fn producer_code(&self) -> &str {
        &self.metadata.producer
    }
    /// Authenticated notice date; this does not admit replay or publication.
    pub fn issue_date(&self) -> NaiveDate {
        self.metadata.date
    }
    pub fn replacement_names(&self) -> &[String] {
        &self.metadata.replacements
    }
    pub fn incoming_namespace_sha384(&self) -> &str {
        self.absence.incoming_exchange().namespace_sha384()
    }
    /// Product metadata equality only. Passing this method grants no deletion.
    /// Cryptographic signature values/signer/graph are intentionally excluded here
    /// and must be bound independently against OriginalDatasetAuthentication.
    pub fn validate_original_nonsignature_metadata(
        &self,
        original: &AuthenticatedS102OriginalMetadata,
    ) -> Result<()> {
        compare(&self.metadata, &original.metadata)
    }
}
/// A conjunction of signed S-102 metadata, exact original signatures and an
/// absent resource in the same owned receiver namespace. This evidence is not
/// producer/delegate authority, replay admission, replacement staging, or removal.
/// Private fields prevent assembling the proof from mutable public reports.
pub struct S102CancellationEvidence<'a> {
    metadata: S102CancellationMetadata<'a>,
    original: ferrite_security::CopiedOriginalSignatureBinding<'a>,
}
impl<'a> S102CancellationEvidence<'a> {
    pub fn bind(
        metadata: S102CancellationMetadata<'a>,
        original: ferrite_security::CopiedOriginalSignatureBinding<'a>,
    ) -> Result<Self> {
        ensure!(
            std::ptr::eq(
                metadata.absence.incoming_exchange().catalogue(),
                original.incoming_catalogue()
            ),
            "Cancellation signature evidence belongs to another incoming namespace"
        );
        let retained = AuthenticatedS102OriginalMetadata::from_authentication(original.original())?;
        metadata.validate_original_nonsignature_metadata(&retained)?;
        Ok(Self { metadata, original })
    }
    pub fn metadata(&self) -> &S102CancellationMetadata<'a> {
        &self.metadata
    }
    pub fn original(&self) -> &ferrite_security::CopiedOriginalSignatureBinding<'a> {
        &self.original
    }
}

fn named(n: Node<'_, '_>, ns: &str, name: &str) -> bool {
    n.is_element() && n.tag_name().namespace() == Some(ns) && n.tag_name().name() == name
}
fn children<'a, 'b>(n: Node<'a, 'b>, ns: &str, name: &str) -> Result<Vec<Node<'a, 'b>>> {
    let all = n
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == name)
        .collect::<Vec<_>>();
    ensure!(
        all.iter().all(|c| named(*c, ns, name)),
        "Wrong namespace for {name}"
    );
    Ok(all)
}
fn one<'a, 'b>(n: Node<'a, 'b>, ns: &str, name: &str) -> Result<Node<'a, 'b>> {
    let all = children(n, ns, name)?;
    ensure!(all.len() == 1, "{name} must occur exactly once");
    Ok(all[0])
}
fn optional<'a, 'b>(n: Node<'a, 'b>, name: &str) -> Result<Option<Node<'a, 'b>>> {
    let all = children(n, XC, name)?;
    ensure!(all.len() <= 1, "Duplicate {name}");
    Ok(all.first().copied())
}
fn value<'a, 'b>(n: Node<'a, 'b>) -> Result<&'a str> {
    ensure!(
        !n.children().any(|c| c.is_element()),
        "Scalar metadata must not contain elements"
    );
    let v = n.text().unwrap_or("").trim();
    ensure!(!v.is_empty(), "Empty metadata value");
    Ok(v)
}
fn string(n: Node<'_, '_>, name: &str) -> Result<String> {
    Ok(value(one(n, XC, name)?)?.into())
}
fn boolean(n: Node<'_, '_>) -> Result<bool> {
    match value(n)? {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => bail!("Invalid Boolean"),
    }
}
fn positive(n: Node<'_, '_>) -> Result<u32> {
    let s = value(n)?;
    ensure!(s.bytes().all(|b| b.is_ascii_digit()), "Invalid integer");
    let x = s.parse()?;
    ensure!(x > 0, "Integer must be positive");
    Ok(x)
}
fn date(n: Node<'_, '_>) -> Result<NaiveDate> {
    let s = value(n)?;
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")?;
    ensure!(d.format("%Y-%m-%d").to_string() == s, "Noncanonical date");
    Ok(d)
}
fn parse_xml_time(s: &str) -> Result<String> {
    ensure!(
        s.is_ascii() && s.len() >= 8,
        "XML time must be ASCII and complete before byte slicing"
    );
    let body = if let Some(body) = s.strip_suffix('Z') {
        body
    } else if s.len() > 8 {
        if let Some(i) = s[8..].find(['+', '-']).map(|i| i + 8) {
            let zone = &s[i + 1..];
            ensure!(
                zone.len() == 5
                    && zone.as_bytes()[2] == b':'
                    && zone[..2].bytes().all(|b| b.is_ascii_digit())
                    && zone[3..].bytes().all(|b| b.is_ascii_digit()),
                "Invalid XML time offset"
            );
            let h: u32 = zone[..2].parse()?;
            let m: u32 = zone[3..].parse()?;
            ensure!(
                h <= 14 && m < 60 && (h != 14 || m == 0),
                "Invalid XML time offset"
            );
            &s[..i]
        } else {
            s
        }
    } else {
        s
    };
    ensure!(
        body.len() >= 8
            && body.is_ascii()
            && body.as_bytes()[2] == b':'
            && body.as_bytes()[5] == b':',
        "Invalid XML time"
    );
    ensure!(
        body[..2].bytes().all(|b| b.is_ascii_digit())
            && body[3..5].bytes().all(|b| b.is_ascii_digit())
            && body[6..8].bytes().all(|b| b.is_ascii_digit()),
        "Time fields must contain decimal digits"
    );
    if body.len() > 8 {
        let fraction = &body[8..];
        ensure!(
            fraction.starts_with('.')
                && fraction.len() > 1
                && fraction[1..].bytes().all(|b| b.is_ascii_digit()),
            "Invalid fractional seconds"
        );
    }
    if let Some(f) = body.strip_prefix("24:00:00") {
        ensure!(
            f.is_empty()
                || (f.starts_with('.') && f.len() > 1 && f[1..].bytes().all(|b| b == b'0')),
            "Invalid end-of-day time"
        );
    } else {
        ensure!(
            body[6..8].parse::<u32>()? < 60,
            "XML time seconds must be less than 60"
        );
        NaiveTime::parse_from_str(body, "%H:%M:%S%.f")?;
        ensure!(
            !body.starts_with("23:59:60"),
            "XML Schema time seconds must be less than 60"
        );
    }
    Ok(s.into())
}
fn real(n: Node<'_, '_>) -> Result<f64> {
    let x: f64 = value(n)?.parse()?;
    ensure!(x.is_finite(), "Nonfinite metadata coordinate/resolution");
    Ok(x)
}
fn filename(s: &str) -> Result<()> {
    let stem = s
        .strip_suffix(".H5")
        .context("S-102 filename must use .H5")?;
    ensure!(
        (7..=19).contains(&stem.len())
            && stem.starts_with("102")
            && stem
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
        "Invalid S-102 dataset basename"
    );
    Ok(())
}
pub(crate) fn validate_resource_uri(uri: &str) -> Result<()> {
    let relative = uri
        .strip_prefix("file:/S-102/DATASET_FILES/")
        .context("Wrong S-102 dataset URI")?;
    ensure!(
        relative.is_ascii()
            && !relative.contains('%')
            && !relative.contains('\\')
            && relative
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != ".."),
        "Unsafe S-102 logical URI"
    );
    filename(relative.rsplit('/').next().unwrap())?;
    Ok(())
}
pub(crate) fn validate_producer_code(producer: &str) -> Result<()> {
    ensure!(
        !producer.is_empty()
            && producer.len() <= 4
            && producer
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
        "Invalid producerCode"
    );
    Ok(())
}

// Bounded expanded-name representation: ignores XML prefixes/attribute ordering
// and element-only indentation. It conservatively retains lexical values and
// child order in complex mandatory fields, rather than silently coercing them.
fn tree(n: Node<'_, '_>) -> Result<String> {
    fn walk(n: Node<'_, '_>, depth: usize, out: &mut String, nodes: &mut usize) -> Result<()> {
        *nodes += 1;
        ensure!(
            depth <= 32 && *nodes <= 65536 && out.len() <= 1024 * 1024,
            "Metadata subtree exceeds policy budget"
        );
        let emit = |s: &str, out: &mut String| {
            out.push_str(&s.len().to_string());
            out.push(':');
            out.push_str(s);
        };
        emit(n.tag_name().namespace().unwrap_or(""), out);
        emit(n.tag_name().name(), out);
        let mut attrs = n
            .attributes()
            .map(|a| (a.namespace().unwrap_or(""), a.name(), a.value()))
            .collect::<Vec<_>>();
        attrs.sort_unstable();
        for (ns, name, v) in attrs {
            out.push('@');
            emit(ns, out);
            emit(name, out);
            emit(v, out);
        }
        out.push('[');
        for c in n.children() {
            if c.is_element() {
                walk(c, depth + 1, out, nodes)?
            } else if c.is_text() {
                let v = c.text().unwrap().trim();
                if !v.is_empty() {
                    out.push('T');
                    emit(v, out);
                }
            }
        }
        out.push(']');
        ensure!(
            out.len() <= 1024 * 1024,
            "Metadata subtree exceeds policy budget"
        );
        Ok(())
    }
    let mut out = String::new();
    walk(n, 0, &mut out, &mut 0)?;
    Ok(out)
}
fn bounding_box(n: Node<'_, '_>) -> Result<()> {
    let v = |name| real(one(one(n, GEX, name)?, GCO, "Decimal")?);
    let (w, e, s, t) = (
        v("westBoundLongitude")?,
        v("eastBoundLongitude")?,
        v("southBoundLatitude")?,
        v("northBoundLatitude")?,
    );
    ensure!(
        (-180.0..=180.0).contains(&w)
            && (-180.0..=180.0).contains(&e)
            && (-90.0..=90.0).contains(&s)
            && (-90.0..=90.0).contains(&t)
            && s <= t,
        "Invalid geographic bounding box"
    );
    Ok(())
}
fn polygon(n: Node<'_, '_>) -> Result<()> {
    let p = one(one(n, GEX, "polygon")?, GML, "Polygon")?;
    let srs = p
        .attribute("srsName")
        .context("Polygon requires EPSG:4326")?;
    ensure!(
        [
            "urn:ogc:def:crs:EPSG::4326",
            "http://www.opengis.net/def/crs/EPSG/0/4326"
        ]
        .contains(&srs),
        "Unsupported polygon SRS"
    );
    ensure!(
        p.attribute((GML, "id")).is_some_and(|v| !v.is_empty()),
        "Polygon requires GML identifier"
    );
    let exterior = one(p, GML, "exterior")?;
    let mut rings = vec![exterior];
    rings.extend(children(p, GML, "interior")?);
    for ring in rings {
        let ring = one(ring, GML, "LinearRing")?;
        let list = children(ring, GML, "posList")?;
        let pos = children(ring, GML, "pos")?;
        ensure!(
            (list.len() == 1 && pos.is_empty()) || (list.is_empty() && pos.len() >= 4),
            "Ring requires posList or >=4 pos"
        );
        let mut points = Vec::new();
        let coords = |text: &str| -> Result<Vec<f64>> {
            let mut a = Vec::new();
            for token in text.split_whitespace() {
                ensure!(
                    a.len() < 131072,
                    "Polygon coordinate budget exceeded before allocation"
                );
                a.try_reserve(1)?;
                a.push(token.parse::<f64>().context("Invalid GML coordinate")?);
            }
            Ok(a)
        };
        if list.len() == 1 {
            let a = coords(value(list[0])?)?;
            ensure!(
                a.len() >= 8 && a.len() % 2 == 0,
                "Polygon position count invalid"
            );
            for pair in a.as_chunks::<2>().0 {
                points.push((pair[0], pair[1]));
            }
        } else {
            for p in pos {
                let a = coords(value(p)?)?;
                ensure!(a.len() == 2, "EPSG4326 pos must have two coordinates");
                points.push((a[0], a[1]));
            }
        }
        ensure!(
            points.len() >= 4 && points.first() == points.last(),
            "Polygon ring must be closed"
        );
        ensure!(
            points.iter().all(|&(lat, lon)| lat.is_finite()
                && lon.is_finite()
                && (-90.0..=90.0).contains(&lat)
                && (-180.0..=180.0).contains(&lon)),
            "Invalid EPSG4326 position"
        );
    }
    Ok(())
}
fn parse(n: Node<'_, '_>) -> Result<Metadata> {
    ensure!(
        named(n, XC, "S100_DatasetDiscoveryMetadata"),
        "Wrong discovery type/namespace"
    );
    for forbidden in [
        "updateNumber",
        "updateApplicationDate",
        "referenceID",
        "temporalExtent",
        "supportFileDiscoveryMetadata",
    ] {
        ensure!(
            children(n, XC, forbidden)?.is_empty(),
            "S-102 does not use {forbidden}"
        );
    }
    let uri = string(n, "fileName")?;
    validate_resource_uri(&uri)?;
    let producer = string(n, "producerCode")?;
    validate_producer_code(&producer)?;
    let purpose = match string(n, "purpose")?.as_str() {
        "1" | "newDataset" => 1,
        "2" | "newEdition" => 2,
        "5" | "cancellation" => 5,
        _ => bail!("Unsupported S-102 purpose"),
    };
    let edition = positive(one(n, XC, "editionNumber")?)?;
    let d = date(one(n, XC, "issueDate")?)?;
    let time = optional(n, "issueTime")?
        .map(|n| parse_xml_time(value(n)?))
        .transpose()?;
    let replaced = optional(n, "replacedData")?.map(boolean).transpose()?;
    let replacements = children(n, XC, "dataReplacement")?
        .into_iter()
        .map(|n| Ok(value(n)?.to_owned()))
        .collect::<Result<Vec<_>>>()?;
    for x in &replacements {
        filename(x)?;
        ensure!(x != uri.rsplit('/').next().unwrap(), "Self replacement");
    }
    let mut unique = std::collections::HashSet::new();
    ensure!(
        replacements.iter().all(|s| unique.insert(s)),
        "Duplicate replacement"
    );
    if purpose == 5 {
        ensure!(
            time.is_some() && replaced.is_some(),
            "Cancellation requires issueTime and replacedData"
        );
        ensure!(
            replaced == Some(!replacements.is_empty()),
            "Replacement declaration inconsistent"
        );
    } else {
        ensure!(
            replaced.is_none() && replacements.is_empty(),
            "Replacement declaration only valid for cancellation"
        );
    }
    let spec = one(n, XC, "productSpecification")?;
    ensure!(
        string(spec, "name")? == "Bathymetric Surface"
            && string(spec, "productIdentifier")? == "S-102"
            && positive(one(spec, XC, "number")?)? == 215,
        "Wrong S-102 product identity"
    );
    ensure!(
        ["3.0", "3.0.0"].contains(&string(spec, "version")?.as_str()),
        "Unsupported S-102 version"
    );
    date(one(spec, XC, "date")?)?;
    ensure!(
        ["HDF5", "3"].contains(&string(n, "encodingFormat")?.as_str()),
        "S-102 requires HDF5 encoding"
    );
    let classification = one(one(n, XC, "classification")?, MCO, "MD_ClassificationCode")?;
    let code = classification
        .attribute("codeListValue")
        .context("Classification requires codeListValue")?;
    ensure!(
        [
            "unclassified",
            "restricted",
            "confidential",
            "secret",
            "topSecret",
            "sensitiveButUnclassified",
            "forOfficialUseOnly",
            "protected",
            "limitedDistribution"
        ]
        .contains(&code),
        "Invalid classification"
    );
    let agency = one(one(n, XC, "producingAgency")?, CIT, "CI_Responsibility")?;
    let role = one(one(agency, CIT, "role")?, CIT, "CI_RoleCode")?;
    ensure!(
        role.attribute("codeListValue")
            .is_some_and(|v| !v.is_empty()),
        "Producing role missing"
    );
    let party = one(one(agency, CIT, "party")?, CIT, "CI_Organisation")?;
    value(one(one(party, CIT, "name")?, GCO, "CharacterString")?)?;
    bounding_box(one(n, XC, "boundingBox")?)?;
    let cov = children(n, XC, "dataCoverage")?;
    ensure!(!cov.is_empty(), "S-102 requires dataCoverage");
    for c in &cov {
        ensure!(
            children(*c, XC, "temporalExtent")?.is_empty(),
            "S-102 coverage has no temporalExtent"
        );
        polygon(one(*c, XC, "boundingPolygon")?)?;
        let r = children(*c, XC, "approximateGridResolution")?;
        ensure!(
            (1..=2).contains(&r.len()),
            "S-102 requires 1..2 grid resolutions"
        );
        for r in r {
            ensure!(real(r)? > 0.0, "Grid resolution must be positive");
        }
        for name in [
            "optimumDisplayScale",
            "maximumDisplayScale",
            "minimumDisplayScale",
        ] {
            if let Some(v) = optional(*c, name)? {
                positive(v)?;
            }
        }
    }
    let nav = children(n, XC, "navigationPurpose")?;
    ensure!(
        (1..=3).contains(&nav.len()),
        "S-102 requires 1..3 navigation purposes"
    );
    let mut navigation = std::collections::HashSet::new();
    for v in nav {
        let x = value(v)?;
        ensure!(
            ["port", "transit", "overview", "1", "2", "3"].contains(&x) && navigation.insert(x),
            "Invalid or duplicate navigation purpose"
        );
    }
    let protected = boolean(one(n, XC, "dataProtection")?)?;
    let scheme = optional(n, "protectionScheme")?;
    ensure!(
        protected == scheme.is_some(),
        "Protection scheme must be present iff encrypted"
    );
    ensure!(
        ["ECDSA-384-SHA2", "8"].contains(&string(n, "digitalSignatureReference")?.as_str()),
        "Unsupported signature algorithm"
    );
    ensure!(
        !children(n, XC, "digitalSignatureValue")?.is_empty(),
        "S-102 requires original signature declarations"
    );
    let mut mandatory = BTreeMap::new();
    for name in [
        "fileName",
        "classification",
        "boundingBox",
        "productSpecification",
        "producingAgency",
        "producerCode",
        "encodingFormat",
    ] {
        mandatory.insert(name.into(), tree(one(n, XC, name)?)?);
    }
    for name in [
        "compressionFlag",
        "dataProtection",
        "copyright",
        "notForNavigation",
    ] {
        mandatory.insert(name.into(), boolean(one(n, XC, name)?)?.to_string());
    }
    mandatory.insert("editionNumber".into(), edition.to_string());
    for name in ["dataCoverage", "navigationPurpose"] {
        let mut items = Vec::new();
        for c in children(n, XC, name)? {
            items.push(tree(c)?);
        }
        mandatory.insert(name.into(), items.join("\n"));
    }
    if let Some(s) = scheme {
        mandatory.insert("protectionScheme".into(), tree(s)?);
    }
    Ok(Metadata {
        uri,
        producer,
        edition,
        date: d,
        time,
        purpose,
        replaced,
        replacements,
        mandatory,
    })
}
fn compare(cancel: &Metadata, original: &Metadata) -> Result<()> {
    ensure!(
        cancel.purpose == 5 && matches!(original.purpose, 1 | 2),
        "Wrong metadata comparison purposes"
    );
    ensure!(
        cancel.uri == original.uri && cancel.mandatory == original.mandatory,
        "Cancellation mandatory metadata differs from original"
    );
    ensure!(
        cancel.date > original.date,
        "Cancellation issueDate must advance original date; replay history still required"
    );
    Ok(())
}
/// Signed metadata/original/absence conjunction plus an explicit local authority
/// grant. This intermediate proof still requires durable replay admission,
/// replacement staging and an atomic product publication transaction.
pub struct S102AuthorizedCancellation<'a, 'p> {
    evidence: S102CancellationEvidence<'a>,
    authority: ferrite_security::CatalogueCancellationAuthority<'p>,
}
impl<'a, 'p> S102AuthorizedCancellation<'a, 'p> {
    pub fn authorize(
        evidence: S102CancellationEvidence<'a>,
        policy: &'p ferrite_security::TrustedCancellationPolicy,
    ) -> Result<Self> {
        // Every scope value comes from the immutable, product-checked metadata;
        // the signer proof comes from the very same owned incoming catalogue.
        let authority = policy.authorize_catalogue(
            evidence.original.incoming_catalogue(),
            "S-102",
            evidence.metadata.producer_code(),
            evidence.metadata.resource_uri(),
        )?;
        Ok(Self {
            evidence,
            authority,
        })
    }
    pub fn evidence(&self) -> &S102CancellationEvidence<'a> {
        &self.evidence
    }
    /// Reauthorize the same retained original against the current local policy
    /// after fallible preparation. Caller time must be trusted, sampled freshly.
    /// This gate does not reverify current PKI/revocation, admit replay or publish.
    pub fn revalidate_local_authority<'q>(
        &self,
        current_original: &ferrite_security::OriginalDatasetAuthentication,
        current_snapshot: &ferrite_security::AuthenticatedSnapshot,
        current_policy: &'q ferrite_security::TrustedCancellationPolicy,
        trusted_now: i64,
    ) -> Result<ferrite_security::CatalogueCancellationAuthority<'q>> {
        ensure!(
            current_original.shares_retained_authentication_with(self.evidence.original.original())
                && std::ptr::eq(current_snapshot, self.evidence.original.resource_snapshot()),
            "S102 cancellation original owner was replaced"
        );
        let authority = current_policy.authorize_catalogue(
            self.evidence.original.incoming_catalogue(),
            "S-102",
            self.evidence.metadata.producer_code(),
            self.evidence.metadata.resource_uri(),
        )?;
        authority.validate_local_window_at(trusted_now)?;
        Ok(authority)
    }
    pub fn authority(&self) -> &ferrite_security::CatalogueCancellationAuthority<'p> {
        &self.authority
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIXTURE: &str = include_str!("discovery_fixture.xml");
    fn entries(xml: &str) -> Result<Vec<Metadata>> {
        let doc = roxmltree::Document::parse(xml)?;
        doc.descendants()
            .filter(|n| named(*n, XC, "S100_DatasetDiscoveryMetadata"))
            .map(parse)
            .collect()
    }
    fn cancel() -> String {
        FIXTURE.replace("<S100XC:purpose>newDataset</S100XC:purpose>","<S100XC:purpose>cancellation</S100XC:purpose>").replace("<S100XC:issueDate>2026-05-26</S100XC:issueDate>","<S100XC:issueDate>2026-05-27</S100XC:issueDate><S100XC:issueTime>18:30:59Z</S100XC:issueTime><S100XC:replacedData>false</S100XC:replacedData>")
    }
    #[test]
    fn public_shom_all_entries_and_metadata_preserving_notice() {
        let old = entries(FIXTURE).unwrap();
        assert_eq!(old.len(), 7);
        let new = entries(&cancel()).unwrap();
        for (a, b) in new.iter().zip(&old) {
            compare(a, b).unwrap();
            assert_eq!(a.edition, 1);
            assert_eq!(a.producer, "FR");
        }
    }
    #[test]
    fn forbidden_s101_update_metadata_rejected() {
        for field in [
            "updateNumber",
            "updateApplicationDate",
            "referenceID",
            "temporalExtent",
        ] {
            let v = FIXTURE.replacen(
                "<S100XC:purpose>",
                &format!("<S100XC:{field}>0</S100XC:{field}><S100XC:purpose>"),
                1,
            );
            assert!(entries(&v).is_err(), "{field}");
        }
    }
    #[test]
    fn registry_version_purpose_and_uri_not_guessed() {
        for (a, b) in [
            ("<S100XC:number>215", "<S100XC:number>102"),
            ("<S100XC:version>3.0", "<S100XC:version>3.1"),
            (
                "<S100XC:productIdentifier>S-102",
                "<S100XC:productIdentifier>S-101",
            ),
            ("<S100XC:purpose>newDataset", "<S100XC:purpose>update"),
            ("SMALOG000001.H5", "SMALOG000001.h5"),
            ("file:/S-102/DATASET_FILES/", "file:/S-101/DATASET_FILES/"),
        ] {
            assert!(entries(&FIXTURE.replace(a, b)).is_err(), "{b}");
        }
    }
    #[test]
    fn missing_duplicates_and_namespace_confusion_fail() {
        for name in [
            "classification",
            "producingAgency",
            "boundingBox",
            "editionNumber",
            "compressionFlag",
            "dataProtection",
            "copyright",
            "notForNavigation",
            "encodingFormat",
        ] {
            let begin = format!("<S100XC:{name}>");
            let end = format!("</S100XC:{name}>");
            let a = FIXTURE.find(&begin).unwrap();
            let b = a + FIXTURE[a..].find(&end).unwrap() + end.len();
            let fragment = &FIXTURE[a..b];
            let mut missing = FIXTURE.to_owned();
            missing.replace_range(a..b, "");
            assert!(entries(&missing).is_err(), "missing{name}");
            let duplicate = FIXTURE.replacen(fragment, &format!("{fragment}{fragment}"), 1);
            assert!(entries(&duplicate).is_err(), "duplicate{name}");
        }
        assert!(entries(&FIXTURE.replacen(
            "<S100XC:editionNumber>",
            "<S100XC:editionNumber xmlns:S100XC=\"urn:wrong\">",
            1
        ))
        .is_err());
    }
    #[test]
    fn cancellation_requires_time_and_explicit_replacement_state() {
        for (a, b) in [
            ("<S100XC:issueTime>18:30:59Z</S100XC:issueTime>", ""),
            ("<S100XC:replacedData>false</S100XC:replacedData>", ""),
            ("<S100XC:replacedData>false", "<S100XC:replacedData>true"),
        ] {
            assert!(entries(&cancel().replace(a, b)).is_err());
        }
        let v=cancel().replace("<S100XC:replacedData>false</S100XC:replacedData>","<S100XC:replacedData>true</S100XC:replacedData><S100XC:dataReplacement>102FR00REPLACEMENT.H5</S100XC:dataReplacement>");
        assert!(entries(&v).is_ok());
    }
    #[test]
    fn xml_time_zones_fraction_end_of_day_and_invalid_inputs() {
        for s in [
            "18:30:59Z",
            "18:30:59+01:00",
            "18:30:59-05:00",
            "18:30:59",
            "18:30:59.125Z",
            "24:00:00Z",
        ] {
            assert!(parse_xml_time(s).is_ok(), "{s}");
        }
        for s in [
            "183059Z",
            "18:30:59++1:00",
            "18:30:59--1:00",
            "+1:30:59Z",
            "18:30:59.Z",
            "18:30:5é",
            "18:30:59+é:00",
            "12:30:60Z",
            "25:00:00Z",
            "18:60:59",
            "18:30:59+14:01",
            "23:59:60Z",
            "18:30:59+aa:00",
            "24:00:01Z",
        ] {
            assert!(parse_xml_time(s).is_err(), "{s}");
        }
    }
    #[test]
    fn geographic_resolution_ring_and_namespace_checks() {
        for (a, b) in [
            (
                "48.62 -2.13 48.62 -2.08 48.67 -2.08 48.67 -2.13 48.62 -2.13",
                "48.62 -2.13 48.62 -2.08 48.67 -2.08 48.67 -2.13 48.63 -2.13",
            ),
            (
                "<S100XC:approximateGridResolution>5",
                "<S100XC:approximateGridResolution>NaN",
            ),
            ("urn:ogc:def:crs:EPSG::4326", "urn:ogc:def:crs:EPSG::3857"),
            ("<gco:Decimal>48.67", "<gco:Decimal>99.0"),
        ] {
            assert!(entries(&FIXTURE.replace(a, b)).is_err(), "{b}");
        }
    }
    #[test]
    fn changed_mandatory_fields_and_replay_fail_comparison() {
        let old = entries(FIXTURE).unwrap();
        for (a, b) in [
            ("<S100XC:producerCode>FR", "<S100XC:producerCode>GB"),
            ("<S100XC:editionNumber>1", "<S100XC:editionNumber>2"),
            ("<S100XC:copyright>true", "<S100XC:copyright>false"),
            (
                "<S100XC:approximateGridResolution>5",
                "<S100XC:approximateGridResolution>6",
            ),
            (
                "<S100XC:navigationPurpose>port",
                "<S100XC:navigationPurpose>transit",
            ),
            (
                "<S100XC:issueDate>2026-05-27",
                "<S100XC:issueDate>2026-05-26",
            ),
        ] {
            let v = entries(&cancel().replace(a, b)).unwrap();
            assert!(compare(&v[0], &old[0]).is_err(), "{b}");
        }
    }
    #[test]
    fn prefix_attribute_order_and_boolean_spellings_do_not_change_result() {
        let old = entries(FIXTURE).unwrap();
        let v = cancel()
            .replace("S100XC:", "xc:")
            .replace("xmlns:S100XC=", "xmlns:xc=")
            .replace(
                "<xc:copyright>true</xc:copyright>",
                "<xc:copyright>1</xc:copyright>",
            );
        let new = entries(&v).unwrap();
        compare(&new[0], &old[0]).unwrap();
    }
}
