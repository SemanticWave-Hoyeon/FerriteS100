//! S-100 Part 15 authentication, independent of products, GUI and transport.
//! Trust anchors are installed independently; embedded certificates never become roots.
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use crypto::{Cert, Curve};
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
mod crypto;
pub use crypto::CryptoBackend;
mod source;
use source::{local_files, SourceHandle};
pub use source::{ExchangeSource, LocalFiles, MemoryExchange};

/// Independently installed scheme administrator roots, verified with one
/// [`CryptoBackend`] (the default backend unless chosen explicitly).
#[derive(Default)]
pub struct TrustAnchors {
    roots: HashMap<String, Cert>,
    backend: CryptoBackend,
}
impl TrustAnchors {
    /// Empty anchors verified by `backend`.
    pub fn with_backend(backend: CryptoBackend) -> Self {
        Self {
            roots: HashMap::new(),
            backend,
        }
    }
    pub fn backend(&self) -> CryptoBackend {
        self.backend
    }
    pub fn install_pem(&mut self, administrator: &str, pem: &[u8]) -> Result<()> {
        ensure!(!administrator.is_empty(), "Empty scheme administrator ID");
        let certificates = Cert::stack_from_pem(self.backend, pem)?;
        ensure!(
            certificates.len() == 1,
            "A trust anchor must contain exactly one certificate"
        );
        let root = certificates.into_iter().next().unwrap();
        ensure!(
            root.verifies_own_signature()?,
            "Trust anchor is not self-signed"
        );
        ensure!(
            root.subject_der() == root.issuer_der(),
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
/// Sources with local files ([`ExchangeSource::file_snapshots`]) get a private
/// temporary file; other sources an owned in-memory copy.
pub struct AuthenticatedSnapshot {
    storage: SnapshotStorage,
    pub source: PathBuf,
}
enum SnapshotStorage {
    File(tempfile::NamedTempFile),
    Memory(std::sync::Arc<[u8]>),
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
    /// The private copy's file. An in-memory snapshot has no file: its path
    /// is empty and opening it fails, so it never redirects to live input.
    pub fn path(&self) -> &Path {
        match &self.storage {
            SnapshotStorage::File(file) => file.path(),
            SnapshotStorage::Memory(_) => Path::new(""),
        }
    }
    /// The authenticated bytes of an in-memory snapshot.
    pub fn bytes(&self) -> Option<&[u8]> {
        match &self.storage {
            SnapshotStorage::File(_) => None,
            SnapshotStorage::Memory(bytes) => Some(bytes),
        }
    }
    /// Whole private copy (file or memory), at most `limit` bytes.
    pub fn read_bytes(&self, limit: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.reader()?
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= limit,
            "Authenticated snapshot exceeds byte limit"
        );
        Ok(bytes)
    }
    /// Independent reader of the private copy (`tempfile::reopen` checks
    /// file identity and does not disturb other readers' cursors).
    fn reader(&self) -> Result<Box<dyn Read + '_>> {
        Ok(match &self.storage {
            SnapshotStorage::File(file) => Box::new(file.reopen()?),
            SnapshotStorage::Memory(bytes) => Box::new(&bytes[..]),
        })
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
        let source = self.source.0.as_ref();
        let mut input = source.open(&self.path)?;
        let mut file = if source.file_snapshots() {
            Some(tempfile::NamedTempFile::new()?)
        } else {
            None
        };
        let mut memory = Vec::new();
        let mut digest = crypto::Sha384::new()?;
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
            match &mut file {
                Some(output) => output.write_all(&buffer[..n])?,
                None => memory.extend_from_slice(&buffer[..n]),
            }
            digest.update(&buffer[..n])?;
        }
        ensure!(
            size == self.size && hex(&digest.finish()?) == self.sha384,
            "Resource changed after signature verification"
        );
        let storage = match file {
            Some(mut output) => {
                output.flush()?;
                SnapshotStorage::File(output)
            }
            None => SnapshotStorage::Memory(memory.into()),
        };
        Ok(AuthenticatedSnapshot {
            storage,
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
    cert: Cert,
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
fn read_xml(source: &dyn ExchangeSource, path: &Path) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    source.open(path)?.take(MAX_XML + 1).read_to_end(&mut b)?;
    ensure!(b.len() as u64 <= MAX_XML, "XML exceeds size limit");
    Ok(b)
}
impl Certificates {
    /// Merge every certificate container of `parent`. Some producers (UKHO 2026)
    /// split certificates over several containers, each repeating the scheme
    /// administrator declaration. Certificate IDs stay globally unique.
    fn parse(parent: Node<'_, '_>, backend: CryptoBackend) -> Result<Self> {
        let ns = parent
            .tag_name()
            .namespace()
            .context("Missing certificate namespace")?;
        let containers: Vec<_> = parent
            .children()
            .filter(|n| is(*n, ns, "certificates"))
            .collect();
        ensure!(!containers.is_empty(), "Missing certificates");
        let mut result = Self {
            members: HashMap::new(),
            administrators: HashSet::new(),
        };
        for container in containers {
            let mut ids = HashSet::new();
            for n in container.children().filter(Node::is_element) {
                let id = attribute(n, "id")?.to_owned();
                ensure!(
                    ids.insert(id.clone()),
                    "Duplicate certificate/administrator ID {id}"
                );
                if is(n, SE, "schemeAdministrator") {
                    ensure!(
                        !result.members.contains_key(&id),
                        "Duplicate certificate/administrator ID {id}"
                    );
                    result.administrators.insert(id);
                } else if is(n, SE, "certificate") {
                    ensure!(
                        !result.administrators.contains(&id) && !result.members.contains_key(&id),
                        "Duplicate certificate/administrator ID {id}"
                    );
                    let cert = Cert::from_der(backend, &decode(n)?)?;
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
        }
        ensure!(
            !result.administrators.is_empty(),
            "Missing scheme administrator"
        );
        Ok(result)
    }
    /// Validate every listed certificate (test helper; production code only
    /// validates certificates that signatures reference).
    #[cfg(test)]
    fn verified(&self, anchors: &TrustAnchors, time: i64) -> Result<HashMap<String, Cert>> {
        self.members
            .keys()
            .map(|id| {
                Ok((
                    id.clone(),
                    self.validate(id, anchors, time, &mut HashSet::new())?,
                ))
            })
            .collect()
    }
    /// S-100 Part 15: each signature names its signer by certificateRef. Only
    /// those signers (and the intermediates on their paths) must chain to an
    /// installed scheme administrator anchor; any failure among them is fatal.
    /// Other listed certificates are not validated and never become usable
    /// signer keys: they are absent from the returned map and reported.
    fn verified_referenced<'s>(
        &self,
        references: impl IntoIterator<Item = &'s str>,
        anchors: &TrustAnchors,
        time: i64,
        warnings: &mut Vec<String>,
    ) -> Result<HashMap<String, Cert>> {
        let mut verified = HashMap::new();
        let mut used = HashSet::new();
        for id in references {
            if !verified.contains_key(id) {
                let cert = self.validate(id, anchors, time, &mut used)?;
                verified.insert(id.to_owned(), cert);
            }
        }
        let mut ignored: Vec<_> = self
            .members
            .keys()
            .filter(|id| !used.contains(id.as_str()))
            .collect();
        ignored.sort();
        warnings.extend(ignored.into_iter().map(|id| {
            format!("Certificate {id} is listed but not referenced by any signature; ignored (not validated, not trusted)")
        }));
        Ok(verified)
    }
    /// Installed anchor of a declared administrator whose subject CN equals an
    /// XML issuer label (some producers write the SA root MRN, not the
    /// administrator ID). Trust still comes only from the installed anchor and
    /// the X.509 issuer name and signature checks below.
    fn administrator_anchor_named<'a>(
        &self,
        label: &str,
        anchors: &'a TrustAnchors,
    ) -> Result<Option<&'a Cert>> {
        let mut found = None;
        for administrator in &self.administrators {
            let Some(root) = anchors.roots.get(administrator) else {
                continue;
            };
            let named = root
                .subject_common_names()
                .iter()
                .any(|cn| cn.as_slice() == label.as_bytes());
            if named {
                ensure!(
                    found.is_none(),
                    "Ambiguous scheme administrator issuer {label}"
                );
                found = Some(root);
            }
        }
        Ok(found)
    }
    fn validate<'s>(
        &'s self,
        id: &str,
        anchors: &TrustAnchors,
        time: i64,
        used: &mut HashSet<&'s str>,
    ) -> Result<Cert> {
        let (leaf_id, leaf) = self
            .members
            .get_key_value(id)
            .with_context(|| format!("Unknown certificateRef {id}"))?;
        let mut chain = Vec::new();
        let mut path = vec![leaf_id.as_str()];
        let mut seen = HashSet::new();
        let mut current: &str = leaf_id;
        let root = loop {
            ensure!(seen.insert(current), "Certificate issuer cycle");
            let item = self
                .members
                .get(current)
                .context("Missing certificate issuer")?;
            let (issuer, is_root) = if self.administrators.contains(&item.issuer) {
                let root = anchors
                    .roots
                    .get(&item.issuer)
                    .with_context(|| format!("Untrusted scheme administrator {}", item.issuer))?;
                (root, true)
            } else if let Some(member) = self.members.get(&item.issuer) {
                (&member.cert, false)
            } else if let Some(root) = self.administrator_anchor_named(&item.issuer, anchors)? {
                (root, true)
            } else {
                bail!(
                    "Missing intermediate certificate {} for {current}",
                    item.issuer
                );
            };
            ensure!(
                item.cert.issuer_der() == issuer.subject_der(),
                "XML issuer does not match X.509 issuer"
            );
            if is_root {
                break issuer;
            }
            chain.push(issuer.clone());
            current = &item.issuer;
            path.push(current);
        };
        crypto::ensure_backend(root, anchors.backend)?;
        if let Err(error) = crypto::verify_path(&leaf.cert, &chain, root, time) {
            bail!("Certificate path validation failed for {id}: {error}");
        }
        used.extend(path);
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
fn resource_path(source: &dyn ExchangeSource, root: &Path, name: &str) -> Result<PathBuf> {
    let name = resource_relative_name(name)?;
    // Joined part by part: under a Windows verbatim root (`\\?\C:\...`, as
    // canonicalization returns) `/` is not a separator.
    let joined = name
        .split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part));
    let path = source
        .canonicalize(&joined)
        .with_context(|| format!("Missing exchange resource {name}"))?;
    ensure!(
        path.starts_with(root),
        "Exchange resource symlink escapes root"
    );
    ensure!(source.is_file(&path), "Exchange resource is not a file");
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
fn require_data_key(cert: &Cert) -> Result<()> {
    let der = cert.der();
    let (rest, parsed) = x509_parser::parse_x509_certificate(der)
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
    ensure!(
        cert.ec_curve()? == Curve::P384,
        "Data signature key is not NIST P-384"
    );
    Ok(())
}
fn verify_resource(
    source: &SourceHandle,
    path: PathBuf,
    signatures: Vec<Signature>,
    certificates: &HashMap<String, Cert>,
) -> Result<VerifiedResource> {
    verify_resource_with_limit(source, path, signatures, certificates, None)
}
fn verify_resource_with_limit(
    source: &SourceHandle,
    path: PathBuf,
    signatures: Vec<Signature>,
    certificates: &HashMap<String, Cert>,
    max_bytes: Option<u64>,
) -> Result<VerifiedResource> {
    if let Some(limit) = max_bytes {
        ensure!(
            source.0.len(&path)? <= limit,
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
    for s in &signatures {
        if !keys.contains_key(&s.certificate) {
            let cert = certificates
                .get(&s.certificate)
                .with_context(|| format!("Unknown certificateRef {}", s.certificate))?;
            require_data_key(cert)?;
            keys.insert(s.certificate.clone(), cert.clone());
        }
        // NIST P-384 ECDSA DER is at most 104 bytes; reject trailing data explicitly.
        ensure!(s.bytes.len() <= 104, "Invalid signature DER length");
        ensure!(
            crypto::canonical_ecdsa_der(keys[&s.certificate].backend(), &s.bytes)?,
            "Trailing or noncanonical signature DER"
        );
    }
    // Every direct ECDSA signature signs the same SHA-384 digest. Stream and hash
    // file bytes once, independent of the number of signatures or providers.
    let mut digest = crypto::Sha384::new()?;
    let mut input = source.0.open(&path)?;
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
            crypto::sha384(&signatures[parent].bytes)?
        } else {
            digest.clone()
        };
        let valid = keys[&s.certificate].verify_ecdsa_digest(&signed, &s.bytes)?;
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
            Ok((id.clone(), std::sync::Arc::<[u8]>::from(cert.der())))
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
                signer_certificate_sha256: cert.sha256_hex()?,
                signer_certificate_der: signer_der[&s.certificate].clone(),
                signature_target: s.target.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let authentication = std::sync::Arc::new(ResourceAuthentication {
        source: source.clone(),
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
    verify_exchange_in(&local_files(), root, anchors, time)
}
/// [`verify_exchange`] over any [`ExchangeSource`]: the same files are read
/// through `source`, and retained snapshots are taken from it.
pub fn verify_exchange_in(
    source: &std::sync::Arc<dyn ExchangeSource>,
    root: impl AsRef<Path>,
    anchors: &TrustAnchors,
    time: i64,
) -> Result<VerificationReport> {
    let handle = SourceHandle(source.clone());
    let root = source.canonicalize(root.as_ref())?;
    let authenticated = verify_exchange_catalogue_in(source, &root, anchors, time)?;
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
    let certificates = Certificates::parse(node, anchors.backend)?;
    let mut resources = Vec::new();
    let mut dataset_discovery = HashMap::new();
    let mut metadata_warnings = Vec::new();
    let mut pending = Vec::new();
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
        let path = resource_path(source.as_ref(), &root, filename)?;
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
        pending.push((entry, filename, path, signatures));
    }
    // Only certificates named by a certificateRef need a valid path; every one
    // of them must validate, and unreferenced ones never become signer keys.
    let certs = certificates.verified_referenced(
        pending
            .iter()
            .flat_map(|(_, _, _, s)| s.iter().map(|s| s.certificate.as_str())),
        anchors,
        time,
        &mut metadata_warnings,
    )?;
    metadata_warnings.extend(authenticated.certificate_warnings.iter().cloned());
    for (entry, filename, path, signatures) in pending {
        let resource = verify_resource(&handle, path, signatures, &certs)?;
        if is(entry, XC, "S100_DatasetDiscoveryMetadata") {
            let discovery = dataset_discovery::parse(entry, filename, &mut metadata_warnings)?;
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
#[cfg(test)]
mod unreferenced_certificate_tests;

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
                let mut input: Box<dyn Read> = match &snapshot.storage {
                    SnapshotStorage::File(_) => Box::new(File::open(snapshot.path())?),
                    SnapshotStorage::Memory(bytes) => Box::new(&bytes[..]),
                };
                let mut digest = crypto::Sha384::new()?;
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
    authorize_datasets_in(&local_files(), paths, anchors, time, policy)
}
/// [`authorize_datasets`] over any [`ExchangeSource`]. Keys are the
/// source's canonical paths; snapshots are private files for sources with
/// [`ExchangeSource::file_snapshots`], owned bytes otherwise.
pub fn authorize_datasets_in(
    source: &std::sync::Arc<dyn ExchangeSource>,
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
        let canonical = source.canonicalize(path)?;
        if out.snapshots.contains_key(&canonical) {
            continue;
        }
        let root = path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .find(|p| {
                source.exists(&p.join("CATALOG.XML")) || source.exists(&p.join("CATALOG.SIGN"))
            });
        if let Some(root) = root.filter(|p| source.exists(&p.join("CATALOG.SIGN"))) {
            let root = source.canonicalize(root)?;
            if !reports.contains_key(&root) {
                let report = verify_exchange_in(source, &root, anchors, time)?;
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
pub use detached::{verify_detached, verify_detached_with, DetachedAlgorithm};

mod catalogue;
pub use catalogue::{
    authorize_catalogue, authorize_catalogue_in, AuthorizedCatalogue, CatalogueDiscovery,
    CatalogueScope,
};

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
    verify_exchange_catalogue, verify_exchange_catalogue_in, AuthenticatedExchangeCatalogue,
    CatalogueDiscoveryView, OriginalEntryView,
};

mod copied_original_signatures;
pub use copied_original_signatures::CopiedOriginalSignatureBinding;
mod incoming_exchange;
pub use incoming_exchange::{
    capture_catalogue_authenticated_incoming_exchange, CatalogueAuthenticatedIncomingExchange,
    IncomingAsciiResourceAbsence, IncomingExchangeLimits,
};
mod trusted_cancellation_authority;
pub use trusted_cancellation_authority::{
    CancellationAuthorityRole, CancellationDatasetScope, CatalogueCancellationAuthority,
    TrustedCancellationGrant, TrustedCancellationPolicy,
};

mod original_retained_storage;
pub use original_retained_storage::retained_original_authentication_storage;
