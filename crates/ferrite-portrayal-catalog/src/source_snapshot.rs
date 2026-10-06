//! Bounded immutable catalogue inputs. Every parser and Lua VM reads these bytes.
use crate::{PCError, PortrayalCatalogue, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const MAX_FILE_BYTES: usize = 50 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 32768;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy)]
struct SnapshotLimits {
    file: usize,
    total: usize,
    entries: usize,
    depth: usize,
}
const DEFAULT_LIMITS: SnapshotLimits = SnapshotLimits {
    file: MAX_FILE_BYTES,
    total: MAX_TOTAL_BYTES,
    entries: MAX_ENTRIES,
    depth: MAX_DEPTH,
};

#[derive(Debug)]
pub struct CatalogueSources {
    root: PathBuf,
    files: BTreeMap<PathBuf, Arc<[u8]>>,
    directories: BTreeSet<PathBuf>,
    digest: [u8; 32],
}
impl CatalogueSources {
    /// Capture once. Symlinks and special files are rejected explicitly; traversal
    /// and read errors propagate, so a partial manifest never enables caching.
    pub fn capture(root: &Path) -> Result<Arc<Self>> {
        Self::capture_with_limits(root, DEFAULT_LIMITS)
    }
    fn capture_with_limits(root: &Path, limits: SnapshotLimits) -> Result<Arc<Self>> {
        if std::fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(invalid("PC snapshot rejects symlink root"));
        }
        let mut files = BTreeMap::new();
        let mut directories = BTreeSet::new();
        let mut pending = vec![(PathBuf::new(), 0usize)];
        let mut total = 0usize;
        let mut entries = 0usize;
        while let Some((relative, depth)) = pending.pop() {
            if depth > limits.depth {
                return Err(invalid("PC snapshot directory depth limit exceeded"));
            }
            directories.insert(relative.clone());
            let mut children = Vec::new();
            for entry in std::fs::read_dir(root.join(&relative))? {
                entries += 1;
                if entries > limits.entries {
                    return Err(invalid("PC snapshot entry limit exceeded"));
                }
                children.push(entry?);
            }
            children.sort_by_key(|e| e.file_name());
            for entry in children {
                let rel = relative.join(entry.file_name());
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    return Err(invalid(format!(
                        "PC snapshot rejects symlink: {}",
                        rel.display()
                    )));
                }
                if kind.is_dir() {
                    pending.push((rel, depth + 1));
                    continue;
                }
                if !kind.is_file() {
                    return Err(invalid(format!(
                        "PC snapshot rejects special file: {}",
                        rel.display()
                    )));
                }
                let limit = limits.file.min(limits.total.saturating_sub(total));
                let mut bytes = Vec::new();
                std::fs::File::open(entry.path())?
                    .take((limit + 1) as u64)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > limit {
                    return Err(invalid(format!(
                        "PC snapshot byte limit exceeded: {} (file 50 MiB, total 512 MiB)",
                        rel.display()
                    )));
                }
                total += bytes.len();
                files.insert(rel, Arc::from(bytes));
            }
        }
        let mut hash = Sha256::new();
        hash.update(b"ferrite-pc-source-map-v1\0");
        for directory in &directories {
            frame(&mut hash, b"directory");
            frame(&mut hash, path_bytes(directory)?.as_bytes());
        }
        for (path, bytes) in &files {
            frame(&mut hash, b"file");
            frame(&mut hash, path_bytes(path)?.as_bytes());
            frame(&mut hash, bytes);
        }
        Ok(Arc::new(Self {
            root: root.to_path_buf(),
            files,
            directories,
            digest: hash.finalize().into(),
        }))
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    pub fn root_path(&self) -> &Path {
        &self.root
    }
    pub fn read_relative(&self, relative: &Path) -> Result<Arc<[u8]>> {
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(invalid("PC source path escapes snapshot"));
        }
        self.files.get(relative).cloned().ok_or_else(|| {
            PCError::ResourceNotFound(format!("PC snapshot input missing: {}", relative.display()))
        })
    }
    pub fn read_path(&self, path: &Path) -> Result<Arc<[u8]>> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| invalid("PC source path is outside snapshot root"))?;
        self.read_relative(relative)
    }
    /// Resolve an SVG resource lexically inside the retained map, with no filesystem access.
    pub fn read_resource(&self, base_directory: &Path, reference: &Path) -> Result<Arc<[u8]>> {
        let path = if reference.is_absolute() {
            reference.to_path_buf()
        } else {
            base_directory.join(reference)
        };
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| invalid("PC resource escapes snapshot root"))?;
        let mut normalized = PathBuf::new();
        for component in relative.components() {
            match component {
                Component::Normal(name) => normalized.push(name),
                Component::CurDir => {}
                Component::ParentDir if normalized.pop() => {}
                _ => return Err(invalid("PC resource escapes snapshot root")),
            }
        }
        self.read_relative(&normalized)
    }

    pub(crate) fn has_directory(&self, path: &Path) -> bool {
        path.strip_prefix(&self.root)
            .ok()
            .is_some_and(|r| self.directories.contains(r))
    }
    pub(crate) fn contains_path(&self, path: &Path) -> bool {
        self.read_path(path).is_ok()
    }
    pub(crate) fn children(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| invalid("PC directory outside snapshot root"))?;
        Ok(self
            .files
            .keys()
            .filter(|p| p.parent() == Some(relative))
            .map(|p| self.root.join(p))
            .collect())
    }
}
fn invalid(message: impl Into<String>) -> PCError {
    PCError::InvalidValue(message.into())
}
fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
fn path_bytes(path: &Path) -> Result<String> {
    path.components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .ok_or_else(|| invalid("PC snapshot path must be UTF-8"))
        })
        .collect::<Result<Vec<_>>>()
        .map(|v| v.join("/"))
}

/// A parsed PC and the exact shared sources from which it was produced.
#[derive(Debug)]
pub struct BoundPortrayalCatalogue {
    pub(crate) catalogue: PortrayalCatalogue,
    pub(crate) sources: Arc<CatalogueSources>,
}
impl std::ops::Deref for BoundPortrayalCatalogue {
    type Target = PortrayalCatalogue;
    fn deref(&self) -> &Self::Target {
        &self.catalogue
    }
}
impl BoundPortrayalCatalogue {
    pub fn sources(&self) -> Arc<CatalogueSources> {
        Arc::clone(&self.sources)
    }
    pub fn source_digest(&self) -> &[u8; 32] {
        self.sources.digest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ferrite-pc-snapshot-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
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
    #[test]
    fn parsed_metadata_and_full_manifest_are_atomic_across_replacement_and_deletion() {
        let temp = Temp::new();
        let metadata = temp.0.join("portrayal_catalogue.xml");
        std::fs::create_dir(temp.0.join("Rules")).unwrap();
        std::fs::write(
            &metadata,
            "<portrayalCatalog productId='S-101' version='A'><foundationMode/><displayPlanes><displayPlane id='OverRadar' order='1'/></displayPlanes></portrayalCatalog>",
        )
        .unwrap();
        std::fs::write(temp.0.join("Rules/main.lua"), "return 'A'").unwrap();
        let a = PortrayalCatalogue::load_bound(&temp.0).unwrap();
        assert!(Arc::ptr_eq(&a.sources(), &a.sources()));
        assert_eq!(
            PortrayalCatalogue::load(&temp.0).unwrap().version,
            a.version
        );
        let bytes_a = a
            .sources()
            .read_relative(Path::new("portrayal_catalogue.xml"))
            .unwrap();
        std::fs::write(
            &metadata,
            "<portrayalCatalog productId='S-101' version='B'><foundationMode/><displayPlanes><displayPlane id='OverRadar' order='1'/></displayPlanes></portrayalCatalog>",
        )
        .unwrap();
        std::fs::write(temp.0.join("Rules/main.lua"), "return 'B'").unwrap();
        let b = PortrayalCatalogue::load_bound(&temp.0).unwrap();
        assert_eq!(a.version, "A");
        assert_eq!(b.version, "B");
        assert_ne!(a.source_digest(), b.source_digest());
        std::fs::remove_dir_all(&temp.0).unwrap();
        assert_eq!(
            a.sources()
                .read_relative(Path::new("portrayal_catalogue.xml"))
                .unwrap()
                .as_ref(),
            bytes_a.as_ref()
        );
        assert_eq!(
            a.sources()
                .read_relative(Path::new("Rules/main.lua"))
                .unwrap()
                .as_ref(),
            b"return 'A'"
        );
        assert_eq!(
            b.sources()
                .read_relative(Path::new("Rules/main.lua"))
                .unwrap()
                .as_ref(),
            b"return 'B'"
        );
        assert!(a.sources().read_relative(Path::new("../main.lua")).is_err());
        assert_eq!(
            a.sources()
                .read_resource(&temp.0.join("Symbols"), Path::new("../Rules/main.lua"))
                .unwrap()
                .as_ref(),
            b"return 'A'"
        );
        assert!(a
            .sources()
            .read_resource(&temp.0, Path::new("../outside"))
            .is_err());
        assert!(a
            .sources()
            .read_relative(Path::new("Rules/absent.lua"))
            .is_err());
    }
    #[test]
    fn capture_limits_and_read_errors_reject_incomplete_snapshots() {
        let temp = Temp::new();
        std::fs::write(temp.0.join("input"), b"12345").unwrap();
        let small = SnapshotLimits {
            file: 4,
            total: 20,
            entries: 3,
            depth: 3,
        };
        assert!(CatalogueSources::capture_with_limits(&temp.0, small).is_err());
        assert!(CatalogueSources::capture_with_limits(
            &temp.0,
            SnapshotLimits {
                file: 20,
                total: 4,
                ..small
            }
        )
        .is_err());
        assert!(CatalogueSources::capture_with_limits(
            &temp.0,
            SnapshotLimits {
                file: 20,
                entries: 0,
                ..small
            }
        )
        .is_err());
        assert!(CatalogueSources::capture(&temp.0.join("missing")).is_err());
        std::fs::remove_file(temp.0.join("input")).unwrap();
        std::fs::create_dir(temp.0.join("child")).unwrap();
        assert!(CatalogueSources::capture_with_limits(
            &temp.0,
            SnapshotLimits { depth: 0, ..small }
        )
        .is_err());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_diagnosed_instead_of_omitted_from_identity() {
        let temp = Temp::new();
        std::fs::write(temp.0.join("inside"), b"inside").unwrap();
        std::os::unix::fs::symlink("inside", temp.0.join("alias")).unwrap();
        assert!(CatalogueSources::capture(&temp.0)
            .unwrap_err()
            .to_string()
            .contains("symlink"));
        assert!(CatalogueSources::capture(&temp.0.join("alias"))
            .unwrap_err()
            .to_string()
            .contains("symlink root"));
    }
}
