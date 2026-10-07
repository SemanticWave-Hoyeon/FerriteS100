//! S-100 Part 15 authentication, independent of products, GUI and transport.
//! Trust anchors are installed independently; embedded certificates never become roots.
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use openssl::{
    hash::{hash, MessageDigest},
    nid::Nid,
    sign::Verifier,
    stack::Stack,
    x509::{
        store::X509StoreBuilder,
        verify::{X509VerifyFlags, X509VerifyParam},
        X509StoreContext, X509,
    },
};
use roxmltree::{Document, Node, ParsingOptions};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};
const SE: &str = "http://www.iho.int/s100/se/5.2";
const XC: &str = "http://www.iho.int/s100/xc/5.2";
const MAX_XML: u64 = 16 * 1024 * 1024;
#[derive(Default)]
pub struct TrustAnchors {
    roots: HashMap<String, X509>,
}
impl TrustAnchors {
    pub fn install_pem(&mut self, administrator: &str, pem: &[u8]) -> Result<()> {
        ensure!(!administrator.is_empty(), "Empty scheme administrator ID");
        let certificates = X509::stack_from_pem(pem)?;
        ensure!(
            certificates.len() == 1,
            "A trust anchor must contain exactly one certificate"
        );
        let root = certificates.into_iter().next().unwrap();
        let key = root.public_key()?;
        ensure!(root.verify(&key)?, "Trust anchor is not self-signed");
        ensure!(
            root.subject_name().to_der()? == root.issuer_name().to_der()?,
            "Trust anchor issuer differs from subject"
        );
        ensure!(
            !self.roots.contains_key(administrator),
            "Duplicate administrator trust anchor"
        );
        self.roots.insert(administrator.to_owned(), root);
        Ok(())
    }
}
#[derive(Debug, Serialize)]
pub struct VerifiedResource {
    pub path: PathBuf,
    pub signature_ids: Vec<String>,
    pub certificate_ids: Vec<String>,
    pub sha384: String,
    pub size: u64,
    #[serde(skip)]
    authentication: std::sync::Arc<ResourceAuthentication>,
}
/// Private immutable input copy retained while a product reader owns the resource.
/// Copying and hashing use constant memory and authenticate the exact copied bytes.
pub struct AuthenticatedSnapshot {
    file: tempfile::NamedTempFile,
    pub source: PathBuf,
}
/// Private frozen input for host corrections when dataset authentication is disabled.
/// This type carries no verification status and must never count as authenticated.
pub struct UnauthenticatedSnapshot {
    file: tempfile::NamedTempFile,
}
impl UnauthenticatedSnapshot {
    pub fn copy(source: &Path) -> Result<Self> {
        let mut input = File::open(source)?;
        let mut file = tempfile::NamedTempFile::new()?;
        std::io::copy(&mut input, &mut file)?;
        file.flush()?;
        Ok(Self { file })
    }
    /// Capture a regular source with a receiver byte budget. The extra byte
    /// detects growth after metadata was checked; std::io::copy is streaming.
    pub fn copy_bounded(source: &Path, max_bytes: u64) -> Result<Self> {
        let input = File::open(source)?;
        let metadata = input.metadata()?;
        ensure!(metadata.is_file(), "Snapshot source is not a regular file");
        ensure!(
            metadata.len() <= max_bytes,
            "Snapshot source exceeds receiver byte budget"
        );
        let read_limit = max_bytes
            .checked_add(1)
            .context("Snapshot byte budget overflow")?;
        let mut input = input.take(read_limit);
        let mut file = tempfile::NamedTempFile::new()?;
        let copied = std::io::copy(&mut input, &mut file)?;
        ensure!(
            copied <= max_bytes,
            "Snapshot grew beyond receiver byte budget"
        );
        file.flush()?;
        Ok(Self { file })
    }
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}
impl AuthenticatedSnapshot {
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}
impl VerifiedResource {
    pub fn snapshot(&self) -> Result<AuthenticatedSnapshot> {
        ensure!(
            self.path == self.authentication.path
                && self.sha384 == self.authentication.sha384
                && self.size == self.authentication.size
                && self.signature_ids.iter().map(String::as_str).eq(self
                    .authentication
                    .signatures
                    .iter()
                    .map(|s| s.id.as_str()))
                && self.certificate_ids.iter().map(String::as_str).eq(self
                    .authentication
                    .signatures
                    .iter()
                    .map(|s| s.certificate_id.as_str())),
            "Resource report differs from retained authentication"
        );
        self.authentication.snapshot()
    }
}
impl ResourceAuthentication {
    fn snapshot(&self) -> Result<AuthenticatedSnapshot> {
        let mut input = File::open(&self.path)?;
        let mut output = tempfile::NamedTempFile::new()?;
        let mut digest = openssl::hash::Hasher::new(MessageDigest::sha384())?;
        let mut buffer = [0u8; 64 * 1024];
        let mut size = 0u64;
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            size = size
                .checked_add(n as u64)
                .context("Snapshot size overflow")?;
            ensure!(
                size <= self.size,
                "Resource changed after signature verification"
            );
            output.write_all(&buffer[..n])?;
            digest.update(&buffer[..n])?;
        }
        ensure!(
            size == self.size && hex(&digest.finish()?) == self.sha384,
            "Resource changed after signature verification"
        );
        output.flush()?;
        Ok(AuthenticatedSnapshot {
            file: output,
            source: self.path.clone(),
        })
    }
}

#[derive(Debug, Serialize)]
pub struct VerificationReport {
    /// Typed discovery bound to the exact authenticated catalogue and resource.
    pub dataset_discovery: HashMap<PathBuf, AuthenticatedDatasetDiscovery>,
    pub catalogue: VerifiedResource,
    pub resources: Vec<VerifiedResource>,
    pub verified_unix_seconds: i64,
    /// No claim of online revocation checking is made by this offline verifier.
    pub revocation_checked: bool,
    pub trust_anchor_sha256: HashMap<String, String>,
    pub metadata_warnings: Vec<String>,
}
#[derive(Clone)]
struct Certificate {
    cert: X509,
    issuer: String,
}
struct Certificates {
    members: HashMap<String, Certificate>,
    administrators: HashSet<String>,
}
struct Signature {
    id: String,
    certificate: String,
    bytes: Vec<u8>,
    target: Option<String>,
}
fn is(n: Node<'_, '_>, ns: &str, name: &str) -> bool {
    n.is_element()
        && (n.tag_name().namespace() == Some(ns)
            || (ns == SE && n.tag_name().namespace() == Some("http://www.iho.int/s100/se/5.1")))
        && n.tag_name().name() == name
}
fn unique_child<'a, 'b>(n: Node<'a, 'b>, ns: &str, name: &str) -> Result<Node<'a, 'b>> {
    let mut nodes = n.children().filter(|n| is(*n, ns, name));
    let result = nodes.next().with_context(|| format!("Missing {name}"))?;
    ensure!(nodes.next().is_none(), "Duplicate {name}");
    Ok(result)
}
fn text<'a>(n: Node<'a, '_>) -> Result<&'a str> {
    ensure!(
        n.children().all(|n| n.is_text()),
        "Unexpected nested markup in {}",
        n.tag_name().name()
    );
    n.text()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("Missing XML text")
}
fn attribute<'a>(n: Node<'a, '_>, name: &str) -> Result<&'a str> {
    n.attribute(name)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("Missing {name} attribute"))
}
fn decode(n: Node<'_, '_>) -> Result<Vec<u8>> {
    let compact: String = text(n)?
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    STANDARD.decode(compact).context("Invalid base64")
}
fn xml(bytes: &[u8]) -> Result<Document<'_>> {
    ensure!(bytes.len() as u64 <= MAX_XML, "XML exceeds size limit");
    Document::parse_with_options(
        std::str::from_utf8(bytes)?,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 200_000,
        },
    )
    .context("Invalid S-100 XML")
}
fn read_xml(path: &Path) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    File::open(path)?.take(MAX_XML + 1).read_to_end(&mut b)?;
    ensure!(b.len() as u64 <= MAX_XML, "XML exceeds size limit");
    Ok(b)
}
impl Certificates {
    fn parse(parent: Node<'_, '_>) -> Result<Self> {
        let container = unique_child(
            parent,
            parent
                .tag_name()
                .namespace()
                .context("Missing certificate namespace")?,
            "certificates",
        )?;
        let mut result = Self {
            members: HashMap::new(),
            administrators: HashSet::new(),
        };
        let mut ids = HashSet::new();
        for n in container.children().filter(Node::is_element) {
            let id = attribute(n, "id")?.to_owned();
            ensure!(
                ids.insert(id.clone()),
                "Duplicate certificate/administrator ID {id}"
            );
            if is(n, SE, "schemeAdministrator") {
                result.administrators.insert(id);
            } else if is(n, SE, "certificate") {
                let cert = X509::from_der(&decode(n)?)?;
                result.members.insert(
                    id,
                    Certificate {
                        cert,
                        issuer: attribute(n, "issuer")?.into(),
                    },
                );
            } else {
                bail!("Unsupported certificate container element");
            }
        }
        ensure!(
            !result.administrators.is_empty(),
            "Missing scheme administrator"
        );
        Ok(result)
    }
    fn verified(&self, anchors: &TrustAnchors, time: i64) -> Result<HashMap<String, X509>> {
        self.members
            .keys()
            .map(|id| Ok((id.clone(), self.validate(id, anchors, time)?)))
            .collect()
    }
    fn validate(&self, id: &str, anchors: &TrustAnchors, time: i64) -> Result<X509> {
        let leaf = self
            .members
            .get(id)
            .with_context(|| format!("Unknown certificateRef {id}"))?;
        let mut chain = Stack::new()?;
        let mut seen = HashSet::new();
        let mut current = id;
        let root = loop {
            ensure!(seen.insert(current), "Certificate issuer cycle");
            let item = self
                .members
                .get(current)
                .context("Missing certificate issuer")?;
            let issuer = if self.administrators.contains(&item.issuer) {
                anchors
                    .roots
                    .get(&item.issuer)
                    .with_context(|| format!("Untrusted scheme administrator {}", item.issuer))?
            } else {
                &self
                    .members
                    .get(&item.issuer)
                    .context("Missing intermediate certificate")?
                    .cert
            };
            ensure!(
                item.cert.issuer_name().to_der()? == issuer.subject_name().to_der()?,
                "XML issuer does not match X.509 issuer"
            );
            if self.administrators.contains(&item.issuer) {
                break issuer;
            }
            chain.push(issuer.clone())?;
            current = &item.issuer;
        };
        let mut store = X509StoreBuilder::new()?;
        store.add_cert(root.clone())?;
        let mut param = X509VerifyParam::new()?;
        param.set_time(time);
        param.set_flags(X509VerifyFlags::CHECK_SS_SIGNATURE)?;
        store.set_param(&param)?;
        let mut context = X509StoreContext::new()?;
        let (valid, error) = context.init(&store.build(), &leaf.cert, &chain, |ctx| {
            let valid = ctx.verify_cert()?;
            Ok((valid, ctx.error().to_string()))
        })?;
        ensure!(valid, "Certificate path validation failed: {error}");
        Ok(leaf.cert.clone())
    }
}
fn parse_signature(n: Node<'_, '_>) -> Result<Signature> {
    Ok(Signature {
        id: attribute(n, "id")?.into(),
        certificate: attribute(n, "certificateRef")?.into(),
        bytes: decode(n)?,
        target: if is(n, SE, "S100_SE_SignatureOnSignature") {
            Some(attribute(n, "signatureRef")?.into())
        } else {
            None
        },
    })
}
/// Resolve the S-100 exchange-relative URI and reject traversal and escaping symlinks.
fn resource_relative_name(name: &str) -> Result<String> {
    let decoded = percent_encoding::percent_decode_str(name)
        .decode_utf8()?
        .replace('\\', "/");
    let name = if let Some(p) = decoded.strip_prefix("file:/") {
        p
    } else {
        ensure!(
            !decoded.contains(':') && !decoded.starts_with('/'),
            "Unsupported resource URI"
        );
        &decoded
    };
    ensure!(
        !name.starts_with('/') && !name.is_empty(),
        "Invalid exchange-relative path"
    );
    ensure!(
        name.split('/').all(|p| !p.is_empty()
            && p != "."
            && p != ".."
            && !p.contains(':')
            && !p.contains('\0')),
        "Unsafe exchange-relative path"
    );
    Ok(name.to_owned())
}
fn resource_path(root: &Path, name: &str) -> Result<PathBuf> {
    let name = resource_relative_name(name)?;
    let path = root
        .join(&name)
        .canonicalize()
        .with_context(|| format!("Missing exchange resource {name}"))?;
    ensure!(
        path.starts_with(root),
        "Exchange resource symlink escapes root"
    );
    ensure!(path.is_file(), "Exchange resource is not a file");
    Ok(path)
}
fn metadata_flag(entry: Node<'_, '_>, name: &str) -> Result<bool> {
    let mut nodes = entry.children().filter(|n| is(*n, XC, name));
    let Some(n) = nodes.next() else {
        return Ok(false);
    };
    ensure!(nodes.next().is_none(), "Duplicate {name}");
    match text(n)? {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => bail!("Invalid {name} boolean"),
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn require_data_key(cert: &X509) -> Result<()> {
    let der = cert.to_der()?;
    let (rest, parsed) = x509_parser::parse_x509_certificate(&der)
        .map_err(|e| anyhow::anyhow!("Invalid certificate DER: {e}"))?;
    ensure!(rest.is_empty(), "Trailing certificate bytes");
    ensure!(
        !parsed.basic_constraints()?.is_some_and(|b| b.value.ca),
        "CA certificates cannot sign data resources"
    );
    if let Some(usage) = parsed.key_usage()? {
        ensure!(
            usage.value.digital_signature(),
            "Certificate does not permit digital signatures"
        );
    }
    let key = cert.public_key()?;
    ensure!(
        key.ec_key()?.group().curve_name() == Some(Nid::SECP384R1),
        "Data signature key is not NIST P-384"
    );
    Ok(())
}
fn verify_resource(
    path: PathBuf,
    signatures: Vec<Signature>,
    certificates: &HashMap<String, X509>,
) -> Result<VerifiedResource> {
    verify_resource_with_limit(path, signatures, certificates, None)
}
fn verify_resource_with_limit(
    path: PathBuf,
    signatures: Vec<Signature>,
    certificates: &HashMap<String, X509>,
    max_bytes: Option<u64>,
) -> Result<VerifiedResource> {
    if let Some(limit) = max_bytes {
        ensure!(
            std::fs::metadata(&path)?.len() <= limit,
            "Signed resource exceeds receiver byte budget"
        );
    }
    ensure!(!signatures.is_empty(), "Missing resource signature");
    let mut ids = HashMap::with_capacity(signatures.len());
    for (i, s) in signatures.iter().enumerate() {
        ensure!(
            ids.insert(s.id.as_str(), i).is_none(),
            "Duplicate signature ID"
        );
    }
    // Build a dependency graph once. Each chained signature has exactly one parent.
    let mut parents = Vec::with_capacity(signatures.len());
    let mut children = vec![Vec::new(); signatures.len()];
    let mut ready = std::collections::VecDeque::new();
    for (i, s) in signatures.iter().enumerate() {
        let parent = if let Some(target) = &s.target {
            let parent = *ids.get(target.as_str()).context("Unknown signatureRef")?;
            children[parent].push(i);
            Some(parent)
        } else {
            ready.push_back(i);
            None
        };
        parents.push(parent);
    }
    ensure!(
        !ready.is_empty(),
        "Signature chain has no direct data signature"
    );
    // Validate each distinct certificate/key once, without weakening chain validation
    // performed by the exchange verifier before this resource is visited.
    let mut keys = HashMap::new();
    let mut parsed_signatures = Vec::with_capacity(signatures.len());
    for s in &signatures {
        if !keys.contains_key(&s.certificate) {
            let cert = certificates
                .get(&s.certificate)
                .with_context(|| format!("Unknown certificateRef {}", s.certificate))?;
            require_data_key(cert)?;
            keys.insert(s.certificate.clone(), cert.public_key()?.ec_key()?);
        }
        // NIST P-384 ECDSA DER is at most 104 bytes; reject trailing data explicitly.
        ensure!(s.bytes.len() <= 104, "Invalid signature DER length");
        let parsed = openssl::ecdsa::EcdsaSig::from_der(&s.bytes)?;
        ensure!(
            parsed.to_der()? == s.bytes,
            "Trailing or noncanonical signature DER"
        );
        parsed_signatures.push(parsed);
    }
    // Every direct ECDSA signature signs the same SHA-384 digest. Stream and hash
    // file bytes once, independent of the number of signatures or providers.
    let mut digest = openssl::hash::Hasher::new(MessageDigest::sha384())?;
    let mut input = File::open(&path)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .context("Resource size overflow")?;
        ensure!(
            max_bytes.is_none_or(|limit| size <= limit),
            "Signed resource grew beyond receiver byte budget"
        );
        digest.update(&buffer[..n])?;
    }
    let digest = digest.finish()?;
    let mut validated = 0;
    while let Some(i) = ready.pop_front() {
        let s = &signatures[i];
        let signed = if let Some(parent) = parents[i] {
            hash(MessageDigest::sha384(), &signatures[parent].bytes)?
        } else {
            digest
        };
        let valid = parsed_signatures[i].verify(&signed, &keys[&s.certificate])?;
        if s.target.is_some() {
            ensure!(valid, "Invalid chained signature {}", s.id);
        } else {
            ensure!(
                valid,
                "Invalid data signature {} for {}",
                s.id,
                path.display()
            );
        }
        validated += 1;
        ready.extend(children[i].iter().copied());
    }
    ensure!(validated == signatures.len(), "Signature reference cycle");
    // Shared once per distinct signer within this resource, not cloned per signature.
    let signer_der = keys
        .keys()
        .map(|id| {
            let cert = certificates.get(id).context("Missing verified signer")?;
            Ok((id.clone(), std::sync::Arc::<[u8]>::from(cert.to_der()?)))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let descriptors = signatures
        .iter()
        .map(|s| {
            let cert = certificates
                .get(&s.certificate)
                .context("Missing verified signer")?;
            Ok(VerifiedSignatureDescriptor {
                id: s.id.clone(),
                certificate_id: s.certificate.clone(),
                der: s.bytes.clone(),
                signer_certificate_sha256: hex(&cert.digest(MessageDigest::sha256())?),
                signer_certificate_der: signer_der[&s.certificate].clone(),
                signature_target: s.target.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let authentication = std::sync::Arc::new(ResourceAuthentication {
        path: path.clone(),
        sha384: hex(&digest),
        size,
        signatures: descriptors,
    });
    Ok(VerifiedResource {
        authentication,
        path,
        sha384: hex(&digest),
        size,
        signature_ids: signatures.iter().map(|s| s.id.clone()).collect(),
        certificate_ids: signatures.iter().map(|s| s.certificate.clone()).collect(),
    })
}
/// Verify exact catalogue bytes before interpreting any dataset metadata.
/// Then authenticate every referenced resource, including compressed/encrypted bytes as delivered.
pub fn verify_exchange(
    root: impl AsRef<Path>,
    anchors: &TrustAnchors,
    time: i64,
) -> Result<VerificationReport> {
    let root = root.as_ref().canonicalize()?;
    let authenticated = verify_exchange_catalogue(&root, anchors, time)?;
    let catalogue = authenticated.catalogue;
    let bytes = authenticated.bytes;
    let trust_anchor_sha256 = authenticated.trust_anchor_sha256;
    let legacy_namespace = authenticated.legacy_namespace;
    let document = xml(&bytes)?;
    let node = document.root_element();
    ensure!(
        is(node, XC, "S100_ExchangeCatalogue"),
        "Unsupported catalogue namespace/type"
    );
    let certs = Certificates::parse(node)?.verified(anchors, time)?;
    let mut resources = Vec::new();
    let mut dataset_discovery = HashMap::new();
    let mut metadata_warnings = Vec::new();
    if legacy_namespace {
        metadata_warnings.push("Standalone signature uses legacy SE5.1 namespace; P-384 and current independently installed root remain required".into());
    }
    let mut paths = HashSet::new();
    let mut signature_ids = HashSet::new();
    for entry in node.descendants().filter(|n| {
        n.tag_name().namespace() == Some(XC)
            && [
                "S100_DatasetDiscoveryMetadata",
                "S100_SupportFileDiscoveryMetadata",
                "S100_CatalogueDiscoveryMetadata",
            ]
            .contains(&n.tag_name().name())
    }) {
        let filename = text(unique_child(entry, XC, "fileName")?)?;
        let algorithm = text(unique_child(entry, XC, "digitalSignatureReference")?)?;
        ensure!(
            algorithm == "ECDSA-384-SHA2" || algorithm == "8",
            "Unsupported signature algorithm {algorithm}"
        );
        let path = resource_path(&root, filename)?;
        ensure!(paths.insert(path.clone()), "Duplicate resource filename");
        let expected_status = if metadata_flag(entry, "dataProtection")? {
            "encrypted"
        } else if metadata_flag(entry, "compressionFlag")? {
            "compressed"
        } else {
            "unencrypted"
        };
        let containers: Vec<_> = entry
            .children()
            .filter(|n| is(*n, XC, "digitalSignatureValue"))
            .collect();
        ensure!(!containers.is_empty(), "Missing digitalSignatureValue");
        let mut signatures = Vec::new();
        for n in containers
            .iter()
            .flat_map(|c| c.children())
            .filter(Node::is_element)
        {
            ensure!(
                is(n, SE, "S100_SE_SignatureOnData")
                    || is(n, SE, "S100_SE_SignatureOnSignature")
                    || is(n, SE, "S100_SE_DigitalSignature"),
                "Unsupported digital signature type"
            );
            if is(n, SE, "S100_SE_SignatureOnData") {
                let status = attribute(n, "dataStatus")?;
                ensure!(
                    ["unencrypted", "compressed", "encrypted"].contains(&status),
                    "Unknown signature dataStatus"
                );
                if status != expected_status {
                    metadata_warnings.push(format!("{filename}: dataStatus={status}, metadata flags imply {expected_status}; signature verified over exact delivered bytes"));
                }
            } else if is(n, SE, "S100_SE_DigitalSignature") {
                metadata_warnings.push(format!("{filename}: legacy generic digital signature; exact delivered bytes verified, dataStatus absent"));
            }
            let signature = parse_signature(n)?;
            ensure!(
                signature_ids.insert(signature.id.clone()),
                "Duplicate catalogue signature ID"
            );
            signatures.push(signature);
        }
        let resource = verify_resource(path, signatures, &certs)?;
        if is(entry, XC, "S100_DatasetDiscoveryMetadata") {
            let discovery = dataset_discovery::parse(entry)?;
            dataset_discovery.insert(
                resource.path.clone(),
                AuthenticatedDatasetDiscovery::bind(
                    discovery,
                    &resource,
                    &catalogue,
                    bytes.clone(),
                    time,
                    trust_anchor_sha256.clone(),
                    filename.to_owned(),
                    entry.range(),
                ),
            );
        }
        resources.push(resource);
    }
    ensure!(
        !resources.is_empty(),
        "No signed resources in exchange catalogue"
    );
    Ok(VerificationReport {
        dataset_discovery,
        catalogue,
        resources,
        verified_unix_seconds: time,
        revocation_checked: false,
        metadata_warnings,
        trust_anchor_sha256: (*trust_anchor_sha256).clone(),
    })
}

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
pub enum UnsignedPolicy {
    Reject,
    Evaluation,
}
pub struct AuthorizedDatasets {
    /// Same canonical keys as snapshots; explicit provenance in every mode.
    pub dataset_discovery: HashMap<PathBuf, DatasetDiscoveryAuthorization>,
    pub snapshots: HashMap<PathBuf, Option<std::sync::Arc<AuthenticatedSnapshot>>>,
    pub signed_count: usize,
    pub unsigned_count: usize,
    pub metadata_warnings: Vec<String>,
}
impl AuthorizedDatasets {
    /// Bind the public maps back to one retained authenticated resource before
    /// a product planner consumes metadata. `canonical` is the original key
    /// captured during authorization; no live source/catalogue is reread.
    pub fn checked_dataset_discovery(
        &self,
        canonical: &Path,
    ) -> Result<&DatasetDiscoveryAuthorization> {
        ensure!(
            canonical.is_absolute()
                && !canonical.components().any(|c| matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )),
            "Dataset discovery needs its retained canonical key"
        );
        let metadata = self
            .dataset_discovery
            .get(canonical)
            .context("Missing dataset discovery authorization")?;
        let snapshot = self
            .snapshots
            .get(canonical)
            .context("Missing aligned dataset snapshot")?;
        match metadata {
            DatasetDiscoveryAuthorization::Authenticated(bound) => {
                let snapshot = snapshot
                    .as_ref()
                    .context("Authenticated discovery has no retained snapshot")?;
                ensure!(
                    bound.resource_path() == canonical && snapshot.source == canonical,
                    "Authenticated discovery resource/snapshot path mismatch"
                );
                let mut input = File::open(snapshot.path())?;
                let mut digest = openssl::hash::Hasher::new(MessageDigest::sha384())?;
                let mut buffer = [0u8; 64 * 1024];
                let mut total = 0u64;
                loop {
                    let n = input.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    total = total
                        .checked_add(n as u64)
                        .context("Snapshot length overflow")?;
                    ensure!(
                        total <= bound.resource_size(),
                        "Retained snapshot exceeds authenticated resource size"
                    );
                    digest.update(&buffer[..n])?;
                }
                ensure!(
                    total == bound.resource_size(),
                    "Retained snapshot length differs from authenticated resource"
                );
                ensure!(
                    hex(&digest.finish()?) == bound.resource_sha384(),
                    "Retained snapshot differs from authenticated discovery resource"
                );
            }
            DatasetDiscoveryAuthorization::SignatureVerificationDisabled
            | DatasetDiscoveryAuthorization::UnsignedEvaluation => {
                ensure!(
                    snapshot.is_none(),
                    "Unauthenticated discovery has an authenticated snapshot"
                );
            }
        }
        Ok(metadata)
    }
}
/// Authenticate a batch once per exchange set, and snapshot only requested resources.
/// Unsigned evaluation datasets remain explicitly unauthenticated.
pub fn authorize_datasets(
    paths: &[PathBuf],
    anchors: &TrustAnchors,
    time: i64,
    policy: UnsignedPolicy,
) -> Result<AuthorizedDatasets> {
    let mut reports: HashMap<PathBuf, VerificationReport> = HashMap::new();
    let mut out = AuthorizedDatasets {
        dataset_discovery: HashMap::new(),
        snapshots: HashMap::new(),
        signed_count: 0,
        unsigned_count: 0,
        metadata_warnings: Vec::new(),
    };
    for path in paths {
        let canonical = path.canonicalize()?;
        if out.snapshots.contains_key(&canonical) {
            continue;
        }
        let root = path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .find(|p| p.join("CATALOG.XML").exists() || p.join("CATALOG.SIGN").exists());
        if let Some(root) = root.filter(|p| p.join("CATALOG.SIGN").exists()) {
            let root = root.canonicalize()?;
            if !reports.contains_key(&root) {
                let report = verify_exchange(&root, anchors, time)?;
                out.metadata_warnings
                    .extend(report.metadata_warnings.iter().cloned());
                reports.insert(root.clone(), report);
            }
            let resource = reports[&root]
                .resources
                .iter()
                .find(|r| r.path == canonical)
                .context("Requested dataset is not signed by its exchange catalogue")?;
            let discovery = reports[&root]
                .dataset_discovery
                .get(&canonical)
                .context("Requested dataset has no authenticated dataset discovery metadata")?
                .clone();
            let snapshot = std::sync::Arc::new(resource.snapshot()?);
            out.dataset_discovery.insert(
                canonical.clone(),
                DatasetDiscoveryAuthorization::Authenticated(discovery),
            );
            out.snapshots.insert(canonical, Some(snapshot));
            out.signed_count += 1;
        } else {
            ensure!(
                matches!(policy, UnsignedPolicy::Evaluation),
                "Unsigned dataset rejected: {}",
                path.display()
            );
            out.dataset_discovery.insert(
                canonical.clone(),
                DatasetDiscoveryAuthorization::UnsignedEvaluation,
            );
            out.snapshots.insert(canonical, None);
            out.unsigned_count += 1;
        }
    }
    Ok(out)
}

mod detached;
pub use detached::{verify_detached, DetachedAlgorithm};

mod catalogue;
pub use catalogue::{authorize_catalogue, AuthorizedCatalogue, CatalogueDiscovery, CatalogueScope};

#[cfg(test)]
mod unverified_input_tests {
    use super::*;
    #[test]
    fn frozen_unsigned_input_is_independent_from_replaced_source() {
        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(b"original").unwrap();
        source.flush().unwrap();
        let snapshot = UnauthenticatedSnapshot::copy(source.path()).unwrap();
        std::fs::write(source.path(), b"replacement").unwrap();
        assert_eq!(std::fs::read(snapshot.path()).unwrap(), b"original");
        assert_eq!(std::fs::read(source.path()).unwrap(), b"replacement");
    }
}

mod original_authentication;
use original_authentication::ResourceAuthentication;
pub use original_authentication::{OriginalDatasetAuthentication, VerifiedSignatureDescriptor};
mod dataset_discovery;
pub use dataset_discovery::{
    AuthenticatedDatasetDiscovery, DatasetDiscovery, DatasetDiscoveryAuthorization,
    DatasetIssueDate, DatasetPurpose,
};

mod exchange_catalogue_authentication;
pub use exchange_catalogue_authentication::{
    verify_exchange_catalogue, AuthenticatedExchangeCatalogue, CatalogueDiscoveryView,
    OriginalEntryView,
};
