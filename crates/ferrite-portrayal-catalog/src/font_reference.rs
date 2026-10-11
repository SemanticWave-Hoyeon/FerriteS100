//! Captured PC font identity, not authority or a complete TrueType validator.
use crate::{BoundPortrayalCatalogue, CatalogueSources, PCError, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};
const MAX_FONTS: usize = 256;
const MAX_FONT_BYTES: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug)]
pub struct BoundFontReference {
    pc_digest: [u8; 32],
    font_digest: [u8; 32],
    render_family_name: Arc<str>,
    reference: String,
    bytes: Arc<[u8]>,
}
impl BoundFontReference {
    pub fn pc_digest(&self) -> &[u8; 32] {
        &self.pc_digest
    }
    pub fn font_digest(&self) -> &[u8; 32] {
        &self.font_digest
    }
    /// Receiver render-resource namespace, not the authored PC reference identifier.
    /// Generated once from the complete immutable PC and font byte digests.
    pub fn render_family_name(&self) -> &Arc<str> {
        &self.render_family_name
    }
    pub fn reference(&self) -> &str {
        &self.reference
    }
    pub fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }
}
/// No system lookup or live filesystem reads. Construct once per immutable PC.
#[derive(Debug)]
pub struct BoundFontDeclarations {
    sources: Arc<CatalogueSources>,
    declarations: BTreeMap<String, (String, String, String)>,
}
impl BoundFontDeclarations {
    pub fn from_catalogue(pc: &BoundPortrayalCatalogue) -> Result<Self> {
        Self::from_sources(pc.sources())
    }
    pub fn from_sources(sources: Arc<CatalogueSources>) -> Result<Self> {
        let xml = sources.read_relative(Path::new("portrayal_catalogue.xml"))?;
        let declarations = parse_declarations(&xml)?;
        Ok(Self {
            sources,
            declarations,
        })
    }
    pub fn resolve(&self, reference: &str) -> Result<BoundFontReference> {
        let (file, kind, format) = self
            .declarations
            .get(reference)
            .ok_or_else(|| PCError::ResourceNotFound(format!("PC FontReference {reference}")))?;
        if kind != "Font" || format != "TTF" {
            return Err(bad("FontReference requires declared Font/TTF"));
        }
        let path = font_path(file)?;
        let bytes = self.sources.read_relative(&path)?;
        if bytes.len() > MAX_FONT_BYTES {
            return Err(bad("FontReference receiver byte budget exceeded"));
        }
        validate_sfnt_directory(&bytes)?;
        let pc_digest = *self.sources.digest();
        let font_digest = Sha256::digest(&bytes).into();
        let render_family_name = render_family_name(&pc_digest, &font_digest);
        Ok(BoundFontReference {
            pc_digest,
            font_digest,
            render_family_name,
            reference: reference.into(),
            bytes,
        })
    }
}
fn render_family_name(pc: &[u8; 32], font: &[u8; 32]) -> Arc<str> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // Old exact byte spelling: "PCFont:" + 64 lowercase hex + ':' + 64 hex.
    let mut name = String::with_capacity(136);
    name.push_str("PCFont:");
    for (index, digest) in [pc, font].into_iter().enumerate() {
        if index != 0 {
            name.push(':');
        }
        for byte in digest {
            name.push(char::from(HEX[usize::from(byte >> 4)]));
            name.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    name.into()
}
fn bad(message: &str) -> PCError {
    PCError::InvalidValue(message.into())
}
fn font_path(file: &str) -> Result<PathBuf> {
    // Narrow receiver filename policy. URI escaping/remote fetch are not supported.
    if file.is_empty()
        || file.len() > 4096
        || file.trim() != file
        || file.contains(['\\', ':', '%', '\0'])
        || !file.ends_with(".ttf")
    {
        return Err(bad("Unsupported PC font filename"));
    }
    let path = Path::new(file);
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(bad("PC font path escapes Fonts"));
    }
    Ok(Path::new("Fonts").join(path))
}
fn parse_declarations(bytes: &[u8]) -> Result<BTreeMap<String, (String, String, String)>> {
    crate::catalogue::preflight_pc_metadata_xml(bytes, 128, 200_000)?;
    let text = std::str::from_utf8(bytes).map_err(|_| bad("PC font metadata is not UTF-8"))?;
    let doc = roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 200_000,
        },
    )
    .map_err(|_| bad("Invalid PC font XML"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "portrayalCatalog"
        || !matches!(
            root.tag_name().namespace(),
            None | Some("http://www.iho.int/S100PortrayalCatalog/5.2")
        )
    {
        return Err(bad("Unsupported PC font metadata profile"));
    }
    let namespace = root.tag_name().namespace();
    // Legacy official PC uses unqualified children; modern qualified children also allowed.
    let own = |n: roxmltree::Node<'_, '_>| {
        n.is_element()
            && (n.tag_name().namespace().is_none() || n.tag_name().namespace() == namespace)
    };
    let mut containers = root
        .children()
        .filter(|n| own(*n) && n.tag_name().name() == "fonts");
    let mut result = BTreeMap::new();
    let Some(container) = containers.next() else {
        return Ok(result);
    };
    if containers.next().is_some() {
        return Err(bad("Duplicate PC Fonts container"));
    }
    let mut metadata_bytes = 0usize;
    for node in container.children().filter(|n| n.is_element()) {
        if !own(node) || node.tag_name().name() != "font" {
            return Err(bad("Foreign PC font declaration"));
        }
        let id = node.attribute("id").unwrap_or("");
        if id.is_empty() || id.len() > 128 || id.trim() != id {
            return Err(bad("Invalid PC font identifier"));
        }
        if result.len() == MAX_FONTS {
            return Err(bad("PC font declaration receiver budget exceeded"));
        }
        let value = |name| -> Result<String> {
            let mut fields = node
                .children()
                .filter(|n| own(*n) && n.tag_name().name() == name);
            let field = fields
                .next()
                .ok_or_else(|| bad("Missing font declaration field"))?;
            if fields.next().is_some() || field.children().any(|n| n.is_element()) {
                return Err(bad("Duplicate or nested font declaration field"));
            }
            // Preserve complete lexical text, including split CDATA; no first-node truncation.
            let mut text = String::new();
            for n in field.children().filter(|n| n.is_text()) {
                let part = n.text().unwrap_or("");
                if text.len().checked_add(part.len()).is_none_or(|v| v > 4096) {
                    return Err(bad("PC font field receiver budget exceeded"));
                }
                text.push_str(part);
            }
            if text.len() > 4096 {
                return Err(bad("PC font field receiver budget exceeded"));
            }
            Ok(text.trim().into())
        };
        let row = (value("fileName")?, value("fileType")?, value("fileFormat")?);
        let added = id
            .len()
            .checked_add(row.0.len())
            .and_then(|n| n.checked_add(row.1.len()))
            .and_then(|n| n.checked_add(row.2.len()))
            .ok_or_else(|| bad("Font metadata size overflow"))?;
        metadata_bytes = metadata_bytes
            .checked_add(added)
            .ok_or_else(|| bad("Font metadata size overflow"))?;
        if metadata_bytes > 64 * 1024 {
            return Err(bad("Font metadata receiver byte budget exceeded"));
        }
        if result.insert(id.into(), row).is_some() {
            return Err(bad("Duplicate PC font identifier"));
        }
    }
    Ok(result)
}
fn validate_sfnt_directory(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 12 || !matches!(&bytes[..4], b"\0\x01\0\0" | b"true") {
        return Err(bad("FontReference is not standalone TrueType SFNT"));
    }
    let tables = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
    let end = tables
        .checked_mul(16)
        .and_then(|v| v.checked_add(12))
        .ok_or_else(|| bad("Font table overflow"))?;
    if tables == 0 || end > bytes.len() {
        return Err(bad("Truncated font table directory"));
    }
    let mut tags = std::collections::BTreeSet::new();
    for table in bytes[12..end].as_chunks::<16>().0 {
        let tag: [u8; 4] = table[..4].try_into().map_err(|_| bad("Invalid font tag"))?;
        if !tags.insert(tag) {
            return Err(bad("Duplicate font table"));
        }
        let offset = u32::from_be_bytes(
            table[8..12]
                .try_into()
                .map_err(|_| bad("Invalid font offset"))?,
        ) as usize;
        let length = u32::from_be_bytes(
            table[12..16]
                .try_into()
                .map_err(|_| bad("Invalid font length"))?,
        ) as usize;
        if offset < end || offset.checked_add(length).is_none_or(|v| v > bytes.len()) {
            return Err(bad("Font table range outside captured bytes"));
        }
    }
    if !tags.contains(b"glyf") || !tags.contains(b"loca") {
        return Err(bad("FontReference requires TrueType outlines"));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    const XML: &str = "<portrayalCatalog><fonts><font id='same'><fileName>font.ttf</fileName><fileType>Font</fileType><fileFormat>TTF</fileFormat></font></fonts></portrayalCatalog>";
    #[test]
    fn declared_identity_not_family_name() {
        let rows = parse_declarations(XML.as_bytes()).unwrap();
        assert!(rows.contains_key("same"));
        assert!(!rows.contains_key("ChartBold"));
    }
    #[test]
    fn duplicate_foreign_and_dtd_reject() {
        for xml in [XML.replace("</fonts>", "<font id='same'><fileName>a.ttf</fileName><fileType>Font</fileType><fileFormat>TTF</fileFormat></font></fonts>"), XML.replace("<font id", "<font xmlns='https://evil.invalid' id"), format!("<!DOCTYPE portrayalCatalog>{XML}")] { assert!(parse_declarations(xml.as_bytes()).is_err()); }
    }
    #[test]
    fn split_text_not_truncated() {
        let xml = XML.replace("font.ttf", "fo<![CDATA[nt]]>.ttf");
        assert_eq!(
            parse_declarations(xml.as_bytes()).unwrap()["same"].0,
            "font.ttf"
        );
    }
    #[test]
    fn no_remote_or_parent_font_path() {
        for p in [
            "../a.ttf",
            "/a.ttf",
            "https://x/a.ttf",
            "a\\b.ttf",
            "a%2fb.ttf",
        ] {
            assert!(font_path(p).is_err());
        }
        assert_eq!(
            font_path("sub/a.ttf").unwrap(),
            Path::new("Fonts/sub/a.ttf")
        );
    }
    #[test]
    fn truncated_and_non_truetype_fail() {
        for b in [
            b"OTTO".as_slice(),
            b"\0\x01\0\0\0\x02\0\0\0\0\0\0".as_slice(),
        ] {
            assert!(validate_sfnt_directory(b).is_err());
        }
    }
}

#[cfg(test)]
mod captured_font_tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new(font: &[u8]) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ferrite-referenced-font-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::create_dir(path.join("Fonts")).unwrap();
            std::fs::write(path.join("portrayal_catalogue.xml"),"<portrayalCatalog><fonts><font id='same'><fileName>font.ttf</fileName><fileType>Font</fileType><fileFormat>TTF</fileFormat></font></fonts></portrayalCatalog>").unwrap();
            std::fs::write(path.join("Fonts/font.ttf"), font).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn same_reference_in_two_pc_owners_is_not_a_global_font_name() {
        let a = Temp::new(include_bytes!(
            "../../../Catalogues/PC/S-421/Fonts/OpenSans-Regular.ttf"
        ));
        let b = Temp::new(include_bytes!(
            "../../../Catalogues/PC/S-421/Fonts/OpenSans-Bold.ttf"
        ));
        let fa = BoundFontDeclarations::from_sources(CatalogueSources::capture(&a.0).unwrap())
            .unwrap()
            .resolve("same")
            .unwrap();
        let fb = BoundFontDeclarations::from_sources(CatalogueSources::capture(&b.0).unwrap())
            .unwrap()
            .resolve("same")
            .unwrap();
        assert_eq!(fa.reference(), fb.reference());
        assert_ne!(fa.pc_digest(), fb.pc_digest());
        assert_ne!(fa.font_digest(), fb.font_digest());
    }
    #[test]
    fn captured_font_survives_replacement_and_missing_id_fails() {
        let temp = Temp::new(include_bytes!(
            "../../../Catalogues/PC/S-421/Fonts/OpenSans-Regular.ttf"
        ));
        let sources = CatalogueSources::capture(&temp.0).unwrap();
        let declarations = BoundFontDeclarations::from_sources(sources).unwrap();
        let original = declarations.resolve("same").unwrap();
        std::fs::write(temp.0.join("Fonts/font.ttf"), b"invalid replaced bytes").unwrap();
        let retained = declarations.resolve("same").unwrap();
        assert!(Arc::ptr_eq(original.bytes(), retained.bytes()));
        assert_eq!(original.font_digest(), retained.font_digest());
        assert!(declarations.resolve("ChartBold").is_err());
        assert!(
            BoundFontDeclarations::from_sources(CatalogueSources::capture(&temp.0).unwrap())
                .unwrap()
                .resolve("same")
                .is_err()
        );
    }
}

#[cfg(test)]
mod family_name_tests {
    use super::*;
    #[test]
    fn full_digest_key_is_byte_exact_to_independent_legacy_formatter() {
        for seed in 0..=255u8 {
            let pc = [seed; 32];
            let font = std::array::from_fn(|i| seed.wrapping_add(i as u8));
            let legacy_hex = |digest: &[u8; 32]| {
                digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            };
            let legacy = format!("PCFont:{}:{}", legacy_hex(&pc), legacy_hex(&font));
            let actual = render_family_name(&pc, &font);
            assert_eq!(actual.as_ref(), legacy);
            assert_eq!(actual.len(), 136);
        }
    }
    #[test]
    fn complete_pc_and_font_ownership_are_both_in_key() {
        let pc = [0; 32];
        let font = [1; 32];
        let base = render_family_name(&pc, &font);
        for i in 0..32 {
            let mut changed_pc = pc;
            changed_pc[i] = 2;
            let mut changed_font = font;
            changed_font[i] = 2;
            assert_ne!(base, render_family_name(&changed_pc, &font));
            assert_ne!(base, render_family_name(&pc, &changed_font));
        }
    }
}
