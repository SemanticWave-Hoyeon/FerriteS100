//! Per-captured-PC cache. No view permission, palette, glyph placement or live file reads.
use crate::{Result, WgpuError};
use ferrite_portrayal_catalog::{BoundFontDeclarations, BoundFontReference, CatalogueSources};
use std::{collections::BTreeMap, sync::Arc};
#[derive(Debug, Default)]
pub(crate) struct FontAssetCache {
    owner: Option<Arc<CatalogueSources>>,
    declarations: Option<BoundFontDeclarations>,
    fonts: BTreeMap<String, BoundFontReference>,
    unique_bytes: usize,
}
impl FontAssetCache {
    pub(crate) fn resolve(
        &mut self,
        owner: Arc<CatalogueSources>,
        reference: &str,
    ) -> Result<BoundFontReference> {
        let bad = |s: &str| WgpuError::Render(s.into());
        if let Some(previous) = &self.owner {
            if !Arc::ptr_eq(previous, &owner) {
                return Err(bad("Font cache captured PC owner changed"));
            }
        } else {
            let declarations = BoundFontDeclarations::from_sources(owner.clone())
                .map_err(|e| WgpuError::Render(e.to_string()))?;
            self.owner = Some(owner);
            self.declarations = Some(declarations);
        }
        if let Some(font) = self.fonts.get(reference) {
            return Ok(font.clone());
        }
        if self.fonts.len() == 32 {
            return Err(bad("PC font cache reference budget exceeded"));
        }
        let font = self
            .declarations
            .as_ref()
            .ok_or_else(|| bad("PC font declarations unavailable"))?
            .resolve(reference)
            .map_err(|e| WgpuError::Render(e.to_string()))?;
        let bytes = if self
            .fonts
            .values()
            .any(|old| Arc::ptr_eq(old.bytes(), font.bytes()))
        {
            self.unique_bytes
        } else {
            self.unique_bytes
                .checked_add(font.bytes().len())
                .ok_or_else(|| bad("PC font cache size overflow"))?
        };
        if bytes > 16 * 1024 * 1024 {
            return Err(bad("PC font cache byte budget exceeded"));
        }
        self.fonts.insert(reference.into(), font.clone());
        self.unique_bytes = bytes;
        Ok(font)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ferrite-pc-font-cache-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::create_dir(path.join("Fonts")).unwrap();
            std::fs::write(path.join("portrayal_catalogue.xml"),"<portrayalCatalog><fonts><font id='font'><fileName>font.ttf</fileName><fileType>Font</fileType><fileFormat>TTF</fileFormat></font></fonts></portrayalCatalog>").unwrap();
            std::fs::write(
                path.join("Fonts/font.ttf"),
                include_bytes!("../../../Catalogues/PC/S-421/Fonts/OpenSans-Regular.ttf"),
            )
            .unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn repeat_resolution_uses_same_capture_and_retains_no_owned_font_vec() {
        let temp = Temp::new();
        let sources = CatalogueSources::capture(&temp.0).unwrap();
        let mut cache = FontAssetCache::default();
        let first = cache.resolve(sources.clone(), "font").unwrap();
        std::fs::write(temp.0.join("Fonts/font.ttf"), b"replaced live file").unwrap();
        let second = cache.resolve(sources.clone(), "font").unwrap();
        assert!(Arc::ptr_eq(first.bytes(), second.bytes()));
        assert!(Arc::ptr_eq(
            first.render_family_name(),
            second.render_family_name()
        ));
        assert_eq!(first.reference(), second.reference());
        assert_eq!(cache.fonts.len(), 1);
        assert_eq!(cache.unique_bytes, first.bytes().len());
        assert!(cache.resolve(sources, "unknown").is_err());
        assert_eq!(cache.fonts.len(), 1);
    }
    #[test]
    fn reused_key_does_not_accept_independently_captured_pc_owner() {
        let temp = Temp::new();
        let a = CatalogueSources::capture(&temp.0).unwrap();
        let b = CatalogueSources::capture(&temp.0).unwrap();
        assert_eq!(a.digest(), b.digest());
        assert!(!Arc::ptr_eq(&a, &b));
        let mut cache = FontAssetCache::default();
        cache.resolve(a, "font").unwrap();
        assert!(cache.resolve(b.clone(), "font").is_err());
        assert!(FontAssetCache::default().resolve(b, "font").is_ok());
    }
}
