//! Discovery/capture only: no authentication or portrayal authority is produced here.
use ferrite_s421::s421::{Profile, MAX_XML_BYTES};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Debug)]
pub(crate) enum RouteInputError {
    Io(String),
    TooLarge,
    Malformed(String),
    UnsupportedDialect(String),
    AuthenticationRequired,
}
impl fmt::Display for RouteInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "S421 input I/O: {e}"),
            Self::TooLarge => write!(f, "S421 input exceeds8MiB receiver limit"),
            Self::Malformed(e) => write!(f, "Malformed S421/XML input: {e}"),
            Self::UnsupportedDialect(ns) => write!(f, "Unsupported S421 namespace: {ns}"),
            Self::AuthenticationRequired => write!(
                f,
                "Authenticated S421 exchange import unavailable; bare GML rejected"
            ),
        }
    }
}
impl std::error::Error for RouteInputError {}
#[derive(Debug, Clone)]
pub(crate) struct CapturedRouteInput {
    path: PathBuf,
    xml: Arc<str>,
    sha256: [u8; 32],
    profile: Profile,
}
impl CapturedRouteInput {
    pub fn path(&self) -> &Path {
        &self.path
    }
    #[cfg(test)]
    pub fn xml(&self) -> &str {
        &self.xml
    }
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
    pub fn profile(&self) -> Profile {
        self.profile
    }
    /// Authentication remains separate and denied; namespace is not signature proof.
    pub fn allowed_xml(&self, require_signatures: bool) -> Result<&str, RouteInputError> {
        if require_signatures {
            Err(RouteInputError::AuthenticationRequired)
        } else {
            Ok(&self.xml)
        }
    }
}
pub(crate) fn capture_if_route(path: &Path) -> Result<Option<CapturedRouteInput>, RouteInputError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| RouteInputError::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(RouteInputError::Io(
            "Expected regular non-symlink file".into(),
        ));
    }
    if meta.len() > MAX_XML_BYTES as u64 {
        return Err(RouteInputError::TooLarge);
    }
    let file = std::fs::File::open(path).map_err(|e| RouteInputError::Io(e.to_string()))?;
    if !file
        .metadata()
        .map_err(|e| RouteInputError::Io(e.to_string()))?
        .is_file()
    {
        return Err(RouteInputError::Io("Opened input is not regular".into()));
    }
    let mut bytes = Vec::new();
    file.take((MAX_XML_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| RouteInputError::Io(e.to_string()))?;
    classify_owned(path.to_path_buf(), bytes)
}
fn classify_owned(
    path: PathBuf,
    bytes: Vec<u8>,
) -> Result<Option<CapturedRouteInput>, RouteInputError> {
    if bytes.len() > MAX_XML_BYTES {
        return Err(RouteInputError::TooLarge);
    }
    let xml = std::str::from_utf8(&bytes).map_err(|e| RouteInputError::Malformed(e.to_string()))?;
    // Pre-DOM bounds include skipped children, not just root semantics.
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut depth = 0usize;
    let mut nodes = 0usize;
    loop {
        use quick_xml::events::Event;
        match reader
            .read_event()
            .map_err(|e| RouteInputError::Malformed(e.to_string()))?
        {
            Event::DocType(_) => return Err(RouteInputError::Malformed("DTD forbidden".into())),
            Event::Start(_) => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| RouteInputError::Malformed("Depth overflow".into()))?;
                nodes += 1;
                if depth > 128 {
                    return Err(RouteInputError::Malformed("XML depth exceeds128".into()));
                }
            }
            Event::Empty(_) => {
                nodes += 1;
                if depth >= 128 {
                    return Err(RouteInputError::Malformed("XML depth exceeds128".into()));
                }
            }
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| RouteInputError::Malformed("Unbalanced end".into()))?;
            }
            Event::Eof => break,
            _ => {
                nodes += 1;
            }
        }
        if nodes > 100_000 {
            return Err(RouteInputError::Malformed("XML nodes exceed100000".into()));
        }
    }
    let doc = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
        },
    )
    .map_err(|e| RouteInputError::Malformed(e.to_string()))?;
    let root = doc.root_element();
    let ns = root.tag_name().namespace().unwrap_or_default();
    let profile = match ns {
        "http://www.iho.int/S421/gml/cs0/1.0" => Profile::Published1,
        "http://www.iec.ch/S421/2.0" => Profile::Candidate2,
        _ => {
            if ns.contains("S421") || ns.contains("S-421") {
                return Err(RouteInputError::UnsupportedDialect(ns.to_owned()));
            }
            return Ok(None);
        }
    };
    if root.tag_name().name() != "Dataset" {
        return Err(RouteInputError::Malformed(
            "S421 root must be Dataset".into(),
        ));
    }
    let sha256 = Sha256::digest(&bytes).into();
    Ok(Some(CapturedRouteInput {
        path,
        xml: Arc::from(xml),
        sha256,
        profile,
    }))
}
/// Extra product discovery; existing numeric/HDF scanner remains byte-identical.
/// Non-S421 XML catalogues are not reclassified as routes.
pub(crate) fn discover_routes(
    root: &Path,
) -> Result<(Vec<CapturedRouteInput>, Vec<String>), RouteInputError> {
    if !std::fs::symlink_metadata(root)
        .map_err(|e| RouteInputError::Io(e.to_string()))?
        .is_dir()
    {
        return Err(RouteInputError::Io("Expected real directory".into()));
    }
    let mut paths = Vec::new();
    for (count, entry) in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if count >= 100_000 {
            return Err(RouteInputError::TooLarge);
        }
        let entry = entry.map_err(|e| RouteInputError::Io(e.to_string()))?;
        if entry.file_type().is_file()
            && (crate::dataset_discovery::is_dataset_file(entry.path(), "gml")
                || crate::dataset_discovery::is_dataset_file(entry.path(), "xml"))
        {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.eq_ignore_ascii_case("CATALOG.XML"))
            {
                continue;
            }
            if paths.len() >= 4096 {
                return Err(RouteInputError::TooLarge);
            }
            paths.push(entry.into_path());
        }
    }
    paths.sort();
    let mut inputs = Vec::new();
    let mut notices = Vec::new();
    let mut total = 0usize;
    for path in paths {
        match capture_if_route(&path) {
            Ok(Some(input)) => {
                let next = total
                    .checked_add(input.xml.len())
                    .ok_or(RouteInputError::TooLarge)?;
                if inputs.len() >= 128 || next > 32 * 1024 * 1024 {
                    notices.push(format!(
                        "S421 discovery retention budget exceeded: {}",
                        path.display()
                    ));
                    continue;
                }
                total = next;
                inputs.push(input);
            }
            Ok(None) => {}
            Err(e) => notices.push(format!("{}: {e}", path.display())),
        }
    }
    Ok((inputs, notices))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn namespace_alias_and_dialects_preserve_original_bytes() {
        for (ns, p) in [
            ("http://www.iho.int/S421/gml/cs0/1.0", Profile::Published1),
            ("http://www.iec.ch/S421/2.0", Profile::Candidate2),
        ] {
            let raw = format!("<x:Dataset xmlns:x=\"{ns}\"/>").into_bytes();
            let expected = Sha256::digest(&raw);
            let input = classify_owned("route.xml".into(), raw.clone())
                .unwrap()
                .unwrap();
            assert_eq!(input.profile(), p);
            assert_eq!(input.xml().as_bytes(), raw);
            assert_eq!(input.sha256().as_slice(), expected.as_slice());
            assert!(input.allowed_xml(true).is_err());
        }
    }
    #[test]
    fn unrelated_catalogue_not_route_and_malformed_typed() {
        assert!(
            classify_owned("fc.xml".into(), b"<FeatureCatalogue/>".to_vec())
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            classify_owned("bad.gml".into(), b"<Dataset".to_vec()),
            Err(RouteInputError::Malformed(_))
        ));
        assert!(matches!(
            classify_owned("dtd.xml".into(), b"<!DOCTYPE x><x/>".to_vec()),
            Err(RouteInputError::Malformed(_))
        ));
    }
    #[test]
    fn unsupported_s421_not_relabelled() {
        assert!(matches!(
            classify_owned(
                "v3.gml".into(),
                br#"<Dataset xmlns="http://www.iec.ch/S421/3.0"/>"#.to_vec()
            ),
            Err(RouteInputError::UnsupportedDialect(_))
        ));
    }
    #[test]
    fn bounded_capture_and_no_path_reopen() {
        let raw = br#"<Dataset xmlns="http://www.iho.int/S421/gml/cs0/1.0"/>"#.to_vec();
        let input = classify_owned("not-on-disk.gml".into(), raw.clone())
            .unwrap()
            .unwrap();
        assert_eq!(input.allowed_xml(false).unwrap().as_bytes(), raw);
        assert!(classify_owned("big.gml".into(), vec![b' '; MAX_XML_BYTES + 1]).is_err());
    }
    #[test]
    fn recursive_route_discovery_is_sorted_excludes_appledouble_and_keeps_invalid_as_notice() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        let raw = br#"<Dataset xmlns="http://www.iho.int/S421/gml/cs0/1.0"/>"#;
        std::fs::write(tmp.path().join("nested/b.gml"), raw).unwrap();
        std::fs::write(tmp.path().join("a.xml"), raw).unwrap();
        std::fs::write(tmp.path().join("._ignored.gml"), raw).unwrap();
        std::fs::write(tmp.path().join("fc.xml"), "<FeatureCatalogue/>").unwrap();
        std::fs::write(tmp.path().join("bad.gml"), "<Dataset").unwrap();
        let (inputs, notices) = discover_routes(tmp.path()).unwrap();
        assert_eq!(inputs.len(), 2);
        assert!(inputs[0].path().ends_with("a.xml"));
        assert!(inputs[1].path().ends_with("nested/b.gml"));
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("bad.gml"));
    }
}
