//! Owned receiver namespace. This proves captured bytes and absence in that
//! immutable namespace, never product cancellation or producer authority.
use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone, Copy)]
pub struct IncomingExchangeLimits {
    pub file_bytes: u64,
    pub total_bytes: u64,
    pub nodes: usize,
    pub path_bytes: usize,
}
impl Default for IncomingExchangeLimits {
    fn default() -> Self {
        Self {
            file_bytes: 64 * 1024 * 1024,
            total_bytes: 512 * 1024 * 1024,
            nodes: 16384,
            path_bytes: 4 * 1024 * 1024,
        }
    }
}
impl IncomingExchangeLimits {
    fn validate(self) -> Result<()> {
        let cap = Self::default();
        ensure!(
            self.file_bytes > 0
                && self.file_bytes <= cap.file_bytes
                && self.total_bytes > 0
                && self.total_bytes <= cap.total_bytes
                && self.nodes > 0
                && self.nodes <= cap.nodes
                && self.path_bytes > 0
                && self.path_bytes <= cap.path_bytes,
            "Invalid incoming exchange receiver limits"
        );
        Ok(())
    }
}
struct CapturedFile {
    bytes: Arc<[u8]>,
    sha384: Vec<u8>,
}

/// No Deserialize or mutable path/map accessors. The temporary catalogue
/// directory is private; only the already authenticated bytes are exposed.
pub struct CatalogueAuthenticatedIncomingExchange {
    catalogue: AuthenticatedExchangeCatalogue,
    _catalogue_files: tempfile::TempDir,
    files: BTreeMap<String, CapturedFile>,
    directories: BTreeSet<String>,
    total_bytes: u64,
    namespace_sha384: String,
}
impl std::fmt::Debug for CatalogueAuthenticatedIncomingExchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogueAuthenticatedIncomingExchange")
            .field("namespace_sha384", &self.namespace_sha384)
            .field("files", &self.files.len())
            .field("total_bytes", &self.total_bytes)
            .finish_non_exhaustive()
    }
}
impl CatalogueAuthenticatedIncomingExchange {
    pub fn catalogue(&self) -> &AuthenticatedExchangeCatalogue {
        &self.catalogue
    }
    pub fn namespace_sha384(&self) -> &str {
        &self.namespace_sha384
    }
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
    /// Bytes captured at reception. This alone does not authenticate a dataset.
    pub fn resource_bytes(&self, uri: &str) -> Result<Option<&[u8]>> {
        let name = resource_relative_name(uri)?;
        Ok(self.files.get(&name).map(|f| f.bytes.as_ref()))
    }
    /// For product profiles whose resource paths are ASCII (including S-102
    /// dataset names). Exact, case and Windows trailing-dot/space aliases all
    /// count as present. A directory at the resource path also counts as present.
    /// Arbitrary Unicode URI absence needs a separate product/filesystem policy.
    pub fn prove_ascii_resource_absent(
        &self,
        uri: &str,
    ) -> Result<IncomingAsciiResourceAbsence<'_>> {
        let name = resource_relative_name(uri)?;
        ensure!(
            name.is_ascii(),
            "ASCII resource absence requires an ASCII logical path"
        );
        let key = portable_ascii_alias_key(&name);
        ensure!(
            !self
                .files
                .keys()
                .chain(self.directories.iter())
                .any(|n| portable_ascii_alias_key(n) == key),
            "Incoming resource or portable filename alias is present"
        );
        Ok(IncomingAsciiResourceAbsence {
            exchange: self,
            relative_name: name,
        })
    }
}
/// Borrowed capability, tied to the same authenticated owned incoming tree.
/// It cannot be constructed from a bool, report, or a later live-path lookup.
pub struct IncomingAsciiResourceAbsence<'a> {
    exchange: &'a CatalogueAuthenticatedIncomingExchange,
    relative_name: String,
}
impl IncomingAsciiResourceAbsence<'_> {
    pub fn relative_name(&self) -> &str {
        &self.relative_name
    }
    pub fn incoming_exchange(&self) -> &CatalogueAuthenticatedIncomingExchange {
        self.exchange
    }
}
fn portable_ascii_alias_key(name: &str) -> String {
    // Protocol targets are ASCII. Unicode case expansions are conservatively
    // treated as collisions; this is not a general Unicode URI normalizer.
    name.replace('\\', "/")
        .split('/')
        .map(|p| p.trim_end_matches(['.', ' ']).to_lowercase().to_uppercase())
        .collect::<Vec<_>>()
        .join("/")
}
fn relative_name(root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(root)?;
    let components = rel
        .components()
        .map(|c| match c {
            std::path::Component::Normal(n) => n.to_str().context("Incoming path is not UTF-8"),
            _ => bail!("Unsafe incoming path component"),
        })
        .collect::<Result<Vec<_>>>()?;
    let name = components.join("/");
    ensure!(
        !name.is_empty() && name.len() <= 4096,
        "Incoming path exceeds receiver limit"
    );
    Ok(name)
}
fn scan(
    root: &Path,
    limits: IncomingExchangeLimits,
    mut visit: impl FnMut(&str, &Path, bool) -> Result<()>,
) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    let mut nodes = 0usize;
    let mut names = 0usize;
    while let Some(dir) = stack.pop() {
        let meta = std::fs::symlink_metadata(&dir)?;
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "Incoming directory is not a plain directory"
        );
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let name = relative_name(root, &path)?;
            nodes = nodes
                .checked_add(1)
                .context("Incoming node count overflow")?;
            names = names
                .checked_add(name.len())
                .context("Incoming path size overflow")?;
            ensure!(
                nodes <= limits.nodes && names <= limits.path_bytes,
                "Incoming namespace exceeds receiver budget"
            );
            let meta = std::fs::symlink_metadata(&path)?;
            ensure!(
                !meta.file_type().is_symlink() && (meta.is_dir() || meta.is_file()),
                "Incoming namespace contains symlink or special file"
            );
            ensure!(
                path.canonicalize()?.starts_with(root),
                "Incoming path escapes root"
            );
            visit(&name, &path, meta.is_dir())?;
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}
fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let input = File::open(path)?;
    let before = input.metadata()?;
    ensure!(
        before.is_file() && before.len() <= limit,
        "Incoming file exceeds receiver budget"
    );
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(usize::try_from(before.len())?)?;
    let mut input = input.take(limit + 1);
    input.read_to_end(&mut bytes)?;
    let after = input.get_ref().metadata()?;
    ensure!(
        bytes.len() as u64 <= limit
            && before.len() == bytes.len() as u64
            && after.len() == before.len()
            && after.modified()? == before.modified()?,
        "Incoming file changed during capture"
    );
    Ok(bytes)
}

/// Capture all plain files/directories within explicit receiver limits, verify a
/// second bounded streaming scan against that captured namespace, then verify
/// CATALOG.SIGN using independent trust anchors. Subsequent source mutations do
/// not change the returned namespace. Other resources are NOT thereby signed.
pub fn capture_catalogue_authenticated_incoming_exchange(
    root: impl AsRef<Path>,
    anchors: &TrustAnchors,
    time: i64,
    limits: IncomingExchangeLimits,
) -> Result<CatalogueAuthenticatedIncomingExchange> {
    limits.validate()?;
    let root = root.as_ref();
    let meta = std::fs::symlink_metadata(root)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "Incoming root must be a plain directory"
    );
    let root = root.canonicalize()?;
    let mut files = BTreeMap::new();
    let mut directories = BTreeSet::new();
    let mut total = 0u64;
    scan(&root, limits, |name, path, dir| {
        if dir {
            ensure!(
                directories.insert(name.to_owned()),
                "Duplicate incoming directory"
            );
        } else {
            let available = limits
                .total_bytes
                .checked_sub(total)
                .context("Incoming total overflow")?;
            let limit = limits.file_bytes.min(available).min(
                if ["CATALOG.XML", "CATALOG.SIGN"].contains(&name) {
                    MAX_XML
                } else {
                    u64::MAX
                },
            );
            let bytes = read_file(path, limit)?;
            total = total
                .checked_add(bytes.len() as u64)
                .context("Incoming total overflow")?;
            let sha384 = hash(MessageDigest::sha384(), &bytes)?.to_vec();
            ensure!(
                files
                    .insert(
                        name.to_owned(),
                        CapturedFile {
                            bytes: bytes.into(),
                            sha384
                        }
                    )
                    .is_none(),
                "Duplicate incoming file"
            );
        }
        Ok(())
    })?;
    let mut seen_files = BTreeSet::new();
    let mut seen_dirs = BTreeSet::new();
    scan(&root, limits, |name, path, dir| {
        if dir {
            ensure!(
                directories.contains(name),
                "Incoming directory changed after capture"
            );
            seen_dirs.insert(name.to_owned());
        } else {
            let retained = files
                .get(name)
                .context("Incoming file added after capture")?;
            let mut input = File::open(path)?;
            let mut digest = openssl::hash::Hasher::new(MessageDigest::sha384())?;
            let mut size = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size = size
                    .checked_add(n as u64)
                    .context("Incoming size overflow")?;
                ensure!(
                    size <= retained.bytes.len() as u64,
                    "Incoming file grew after capture"
                );
                digest.update(&buffer[..n])?;
            }
            ensure!(
                size == retained.bytes.len() as u64 && digest.finish()?.as_ref() == retained.sha384,
                "Incoming file differs from captured bytes"
            );
            seen_files.insert(name.to_owned());
        }
        Ok(())
    })?;
    ensure!(
        seen_files.len() == files.len() && seen_dirs == directories,
        "Incoming namespace shrank during capture"
    );
    let private = tempfile::Builder::new()
        .prefix("ferrite-incoming-catalogue-")
        .tempdir()?;
    for name in ["CATALOG.XML", "CATALOG.SIGN"] {
        let file = files
            .get(name)
            .context("Incoming exchange lacks catalogue/signature")?;
        std::fs::write(private.path().join(name), &file.bytes)?;
    }
    let catalogue = verify_exchange_catalogue(private.path(), anchors, time)?;
    let mut digest = openssl::hash::Hasher::new(MessageDigest::sha384())?;
    digest.update(b"ferrite-owned-incoming-namespace-v1\0")?;
    for name in &directories {
        digest.update(b"D")?;
        digest.update(&(name.len() as u64).to_be_bytes())?;
        digest.update(name.as_bytes())?;
    }
    for (name, file) in &files {
        digest.update(b"F")?;
        digest.update(&(name.len() as u64).to_be_bytes())?;
        digest.update(name.as_bytes())?;
        digest.update(&(file.bytes.len() as u64).to_be_bytes())?;
        digest.update(&file.sha384)?;
    }
    Ok(CatalogueAuthenticatedIncomingExchange {
        catalogue,
        _catalogue_files: private,
        files,
        directories,
        total_bytes: total,
        namespace_sha384: hex(&digest.finish()?),
    })
}
