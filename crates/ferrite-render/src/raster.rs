//! Product-neutral georeferenced raster layer; pixels are north-to-south RGBA.
use crate::{DisplayPlane, GeoBounds};
use ferrite_kernel::CompositionStage;

/// Ordinary PC order within a composition stage; not an interoperability catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterDrawOrder {
    pub stage: CompositionStage,
    pub display_plane: DisplayPlane,
    pub priority: i32,
}
impl Default for RasterDrawOrder {
    fn default() -> Self {
        Self {
            stage: CompositionStage::Overlay,
            display_plane: DisplayPlane::UnderRadar,
            priority: 0,
        }
    }
}
impl RasterDrawOrder {
    /// Level 0 retains its separate overlay stage. IC-governed layers can
    /// share Chart stage with vector instructions and interleave by signed order.
    pub fn render_key(self) -> (ferrite_kernel::CompositionPlane, i32) {
        (
            self.display_plane.composition_plane(self.stage),
            self.priority,
        )
    }
}

/// Shared source lattice, in west-to-east/north-to-south pixel orientation.
#[derive(Debug, Clone, Copy)]
pub struct RasterGrid {
    pub bounds: GeoBounds,
    pub width: u32,
    pub height: u32,
    pub column: u32,
    pub row: u32,
}
#[derive(Debug)]
pub struct RasterLayer {
    pub draw_order: RasterDrawOrder,
    /// Empty for legacy ungrouped layers. All assigned PC/IC groups must be enabled.
    pub viewing_groups: Vec<u32>,
    pub id: String,
    pub bounds: GeoBounds,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub grid: Option<RasterGrid>,
}

/// S-100 9-11.1.3: any disabled assigned group disables the entire command.
/// None means all groups enabled; an empty layer list remains ungrouped.
pub fn raster_groups_visible(
    groups: &[u32],
    enabled: Option<&std::collections::HashSet<u32>>,
) -> bool {
    enabled.is_none_or(|enabled| groups.iter().all(|g| enabled.contains(g)))
}
#[cfg(test)]
mod visibility_tests {
    use super::*;
    use std::collections::HashSet;
    #[test]
    fn all_assigned_groups_must_be_enabled() {
        let both = HashSet::from([13030, 90020]);
        let first = HashSet::from([13030]);
        assert!(raster_groups_visible(&[13030, 90020], None));
        assert!(raster_groups_visible(&[13030, 90020], Some(&both)));
        assert!(!raster_groups_visible(&[13030, 90020], Some(&first)));
        assert!(!raster_groups_visible(&[13030], Some(&HashSet::new())));
        assert!(raster_groups_visible(&[], Some(&HashSet::new())));
    }
}

/// Product-neutral original-candidate material. This is not a centroid RGBA image.
/// The existing RasterLayer struct/API remains unchanged. Construction validates
/// the bounded packed-atlas contract; GPU viewport qualification is a separate gate.
#[derive(Debug)]
pub struct ContinuousRasterLayer {
    atlas: RasterLayer,
    metadata: ContinuousRasterMetadata,
}
#[derive(Debug, Clone, Copy)]
pub struct ContinuousRasterMetadata {
    pub logical_size: [u32; 2],
    pub sources: u32,
    pub words: u32,
    pub coordinate_rounding: f64,
    pub maximum_domain_coordinate: f64,
    pub content_digest: [u8; 32],
}
impl ContinuousRasterLayer {
    pub fn new(atlas: RasterLayer) -> anyhow::Result<Self> {
        use anyhow::{ensure, Context};
        let texels = (atlas.width as usize).checked_mul(atlas.height as usize);
        ensure!(
            atlas.width >= 16 && atlas.height > 0 && texels.is_some_and(|n| n <= 4 * 1024 * 1024),
            "Continuous atlas exceeds 4M-texel budget"
        );
        ensure!(
            texels.and_then(|n| n.checked_mul(4)) == Some(atlas.rgba.len()),
            "Invalid continuous atlas byte length"
        );
        let word = |i: usize| u32::from_le_bytes(atlas.rgba[i * 4..i * 4 + 4].try_into().unwrap());
        ensure!(
            atlas.rgba.len() >= 16 * 4,
            "Missing continuous material header"
        );
        ensure!(
            word(0) == 0x53444331 && word(1) == 1,
            "Unknown continuous material format"
        );
        let sources = word(2);
        let words = word(11);
        ensure!(
            (1..=32).contains(&sources)
                && words as usize <= atlas.rgba.len() / 4
                && words as usize >= 16 + 16 * sources as usize,
            "Invalid continuous material table bounds"
        );
        ensure!(
            word(9) == 16 && word(13) == 0 && word(14) == 0,
            "Continuous payload must not claim a viewport qualification"
        );
        let grid = atlas
            .grid
            .context("Continuous material requires a shared grid")?;
        ensure!(
            grid.width <= 1048576 && grid.height <= 1048576,
            "Continuous source coordinate limit exceeded"
        );
        ensure!(
            [grid.width, grid.height, grid.column, grid.row]
                == [word(3), word(4), word(5), word(6)],
            "Continuous atlas/grid mismatch"
        );
        let logical_size = [word(7), word(8)];
        ensure!(
            !logical_size.contains(&0)
                && logical_size[0] as u64 * logical_size[1] as u64 <= 65536
                && grid
                    .column
                    .checked_add(logical_size[0])
                    .is_some_and(|v| v <= grid.width)
                && grid
                    .row
                    .checked_add(logical_size[1])
                    .is_some_and(|v| v <= grid.height),
            "Invalid continuous logical tile"
        );
        let coordinate_rounding = f64::from(f32::from_bits(word(10)));
        let maximum_domain_coordinate = f64::from(f32::from_bits(word(12)));
        ensure!(
            coordinate_rounding.is_finite()
                && coordinate_rounding >= 0.
                && maximum_domain_coordinate.is_finite()
                && maximum_domain_coordinate >= 0.,
            "Invalid continuous coordinate bounds"
        );
        let mut nodes = 0u64;
        let mut vertices = 0u64;
        for source in 0..sources as usize {
            let h = 16 + 16 * source;
            let ratio = [word(h), word(h + 1)];
            let size = [word(h + 6), word(h + 7)];
            ensure!(
                !ratio.contains(&0)
                    && !size.contains(&0)
                    && word(h + 2) > 0
                    && word(h + 3) > 0
                    && grid.width as u64 == word(h + 2) as u64 * ratio[0] as u64
                    && grid.height as u64 == word(h + 3) as u64 * ratio[1] as u64,
                "Invalid original source lattice"
            );
            ensure!(
                word(h + 4)
                    .checked_add(size[0])
                    .is_some_and(|v| v <= word(h + 2))
                    && word(h + 5)
                        .checked_add(size[1])
                        .is_some_and(|v| v <= word(h + 3)),
                "Original source window outside grid"
            );
            let count = size[0] as u64 * size[1] as u64;
            nodes += count;
            let vertex_count = word(h + 10) as u64;
            vertices += vertex_count;
            ensure!(
                nodes <= 2 * 1024 * 1024
                    && vertices <= 4096
                    && (vertex_count == 0 || vertex_count >= 3),
                "Continuous aggregate candidate/domain budget exceeded"
            );
            ensure!(
                word(h + 8) as u64 >= 16 + 16 * sources as u64
                    && word(h + 8) as u64 + count * 2 <= words as u64
                    && word(h + 9) as u64 >= 16 + 16 * sources as u64
                    && word(h + 9) as u64 + vertex_count * 2 <= words as u64,
                "Original source payload outside atlas"
            );
            ensure!(
                word(h + 11) <= 3 && word(h + 12) == source as u32,
                "Invalid original source identity/orientation"
            );
            for i in 0..vertex_count as usize * 2 {
                let coordinate = f64::from(f32::from_bits(word(word(h + 9) as usize + i)));
                ensure!(
                    coordinate.is_finite() && coordinate.abs() <= maximum_domain_coordinate,
                    "Encoded domain coordinate exceeds stated envelope"
                );
            }
        }
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(b"continuous-material-v1");
        hash.update(&atlas.rgba);
        for value in [
            atlas.bounds.min_x,
            atlas.bounds.min_y,
            atlas.bounds.max_x,
            atlas.bounds.max_y,
            grid.bounds.min_x,
            grid.bounds.min_y,
            grid.bounds.max_x,
            grid.bounds.max_y,
        ] {
            hash.update(value.to_bits().to_le_bytes());
        }
        for value in [
            grid.width,
            grid.height,
            grid.column,
            grid.row,
            atlas.width,
            atlas.height,
        ] {
            hash.update(value.to_le_bytes());
        }
        let metadata = ContinuousRasterMetadata {
            logical_size,
            sources,
            words,
            coordinate_rounding,
            maximum_domain_coordinate,
            content_digest: hash.finalize().into(),
        };
        Ok(Self { atlas, metadata })
    }
    pub fn metadata(&self) -> ContinuousRasterMetadata {
        self.metadata
    }
    pub fn into_parts(self) -> (RasterLayer, ContinuousRasterMetadata) {
        (self.atlas, self.metadata)
    }
}

/// Mixed regular/continuous upload batches preserve a single transactional commit.
pub enum RasterMaterialLayer {
    Regular(RasterLayer),
    Continuous(ContinuousRasterLayer),
}
impl From<RasterLayer> for RasterMaterialLayer {
    fn from(value: RasterLayer) -> Self {
        Self::Regular(value)
    }
}
impl From<ContinuousRasterLayer> for RasterMaterialLayer {
    fn from(value: ContinuousRasterLayer) -> Self {
        Self::Continuous(value)
    }
}

#[cfg(test)]
mod continuous_material_tests {
    use super::*;
    fn input() -> RasterLayer {
        let mut words = vec![0u32; 48];
        words[..10].copy_from_slice(&[0x53444331, 1, 1, 1, 1, 0, 0, 1, 1, 16]);
        words[11] = 34;
        words[16..29].copy_from_slice(&[1, 1, 1, 1, 0, 0, 1, 1, 32, 32, 0, 0, 0]);
        words[32] = 0;
        words[33] = 0x80402010;
        RasterLayer {
            draw_order: RasterDrawOrder::default(),
            viewing_groups: vec![13030],
            id: "retained-selector".into(),
            bounds: GeoBounds::new(0., 0., 1., 1.),
            width: 16,
            height: 3,
            rgba: words.into_iter().flat_map(u32::to_le_bytes).collect(),
            grid: Some(RasterGrid {
                bounds: GeoBounds::new(0., 0., 1., 1.),
                width: 1,
                height: 1,
                column: 0,
                row: 0,
            }),
        }
    }
    #[test]
    fn wrapper_preserves_bytes_and_separates_physical_atlas_from_logical_grid() {
        let layer = input();
        let bytes = layer.rgba.clone();
        let continuous = ContinuousRasterLayer::new(layer).unwrap();
        assert_eq!(continuous.metadata().logical_size, [1, 1]);
        let (payload, _) = continuous.into_parts();
        assert_eq!(payload.rgba, bytes);
        assert_eq!([payload.width, payload.height], [16, 3]);
        assert_eq!(&payload.rgba[33 * 4..34 * 4], &[0x10, 0x20, 0x40, 0x80]);
    }
    #[test]
    fn material_identity_binds_candidate_bytes_and_native_geometry() {
        let baseline = ContinuousRasterLayer::new(input())
            .unwrap()
            .metadata()
            .content_digest;
        let mut layer = input();
        layer.rgba[33 * 4] ^= 1;
        assert_ne!(
            baseline,
            ContinuousRasterLayer::new(layer)
                .unwrap()
                .metadata()
                .content_digest
        );
        let mut layer = input();
        layer.grid.as_mut().unwrap().bounds.max_x = 2.;
        assert_ne!(
            baseline,
            ContinuousRasterLayer::new(layer)
                .unwrap()
                .metadata()
                .content_digest
        );
        let mut layer = input();
        layer.bounds.max_x = 2.;
        assert_ne!(
            baseline,
            ContinuousRasterLayer::new(layer)
                .unwrap()
                .metadata()
                .content_digest
        );
    }
    #[test]
    fn aggregate_source_budget_and_unbounded_dimensions_are_rejected() {
        let mut layer = input();
        layer.width = u32::MAX;
        layer.height = u32::MAX;
        assert!(ContinuousRasterLayer::new(layer).is_err());
        let mut layer = input();
        layer.rgba[2 * 4..3 * 4].copy_from_slice(&33u32.to_le_bytes());
        assert!(ContinuousRasterLayer::new(layer).is_err());
        let mut layer = input();
        layer.rgba[10 * 4..11 * 4].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
        assert!(ContinuousRasterLayer::new(layer).is_err());
    }
    #[test]
    fn incomplete_payload_and_claimed_frame_proof_are_rejected() {
        let mut layer = input();
        layer.rgba[14 * 4..15 * 4].copy_from_slice(&1u32.to_le_bytes());
        assert!(ContinuousRasterLayer::new(layer).is_err());
        let mut layer = input();
        layer.rgba[24 * 4..25 * 4].copy_from_slice(&1000u32.to_le_bytes());
        assert!(ContinuousRasterLayer::new(layer).is_err());
        let mut layer = input();
        layer.grid.as_mut().unwrap().column = 1;
        assert!(ContinuousRasterLayer::new(layer).is_err());
    }
}
