//! Separate retained inventory bounds; not a total process/GPU-memory promise.
//! Old and unpublished inventories can coexist; these are texture/buffer totals.
#[derive(Default, Clone, Copy)]
pub(crate) struct RasterPublicationBudget {
    texture: u64,
    geometry: u64,
    identity: u64,
    layers: usize,
}
impl RasterPublicationBudget {
    pub(crate) fn admit(
        &mut self,
        texture: u64,
        geometry: u64,
        identity: u64,
    ) -> crate::Result<()> {
        let next = Self {
            texture: self.texture.checked_add(texture).ok_or_else(Self::error)?,
            geometry: self
                .geometry
                .checked_add(geometry)
                .ok_or_else(Self::error)?,
            identity: self
                .identity
                .checked_add(identity)
                .ok_or_else(Self::error)?,
            layers: self.layers.checked_add(1).ok_or_else(Self::error)?,
        };
        if next.texture > 256 * 1024 * 1024
            || next.geometry > 64 * 1024 * 1024
            || next.identity > 64 * 1024 * 1024
            || next.layers > 4096
        {
            return Err(Self::error());
        }
        *self = next;
        Ok(())
    }
    fn error() -> crate::WgpuError {
        crate::WgpuError::Render(
            "Raster inventory exceeds texture256MiB/geometry64MiB/identity64MiB/4096-layer budget"
                .into(),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn combined_retained_and_new_resources_are_admitted_atomically() {
        let mut b = RasterPublicationBudget::default();
        b.admit(192 * 1024 * 1024, 48 * 1024 * 1024, 48 * 1024 * 1024)
            .unwrap();
        let before = b;
        b.admit(64 * 1024 * 1024, 16 * 1024 * 1024, 16 * 1024 * 1024)
            .unwrap();
        assert!(b.admit(1, 0, 0).is_err());
        assert_eq!(b.texture, 256 * 1024 * 1024);
        let mut geometry = before;
        assert!(geometry.admit(1, 17 * 1024 * 1024, 0).is_err());
        assert_eq!(geometry.geometry, before.geometry);
        let mut identity = before;
        assert!(identity.admit(1, 0, 17 * 1024 * 1024).is_err());
        assert_eq!(identity.identity, before.identity);
    }
    #[test]
    fn layer_capacity_and_overflow_cannot_erase_previous_admission() {
        let mut b = RasterPublicationBudget::default();
        for _ in 0..4096 {
            b.admit(4, 0, 0).unwrap();
        }
        assert!(b.admit(0, 0, 0).is_err());
        assert_eq!(b.layers, 4096);
        let mut b = RasterPublicationBudget::default();
        b.admit(4, 8, 16).unwrap();
        assert!(b.admit(u64::MAX, 0, 0).is_err());
        assert_eq!(b.texture, 4);
        assert_eq!(b.geometry, 8);
        assert_eq!(b.identity, 16);
    }
}
