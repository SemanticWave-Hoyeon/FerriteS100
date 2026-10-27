//! The registered product layers of a chart state (`docs/ENGINE_DESIGN.md`,
//! "제품 모듈화", step 5): a registry of `Box<dyn ProductLayer>` in
//! registration order, each with the products it serves and their scene
//! role. The engine composes them generically (open jobs, installs,
//! reports, portrayal by role, picks); hosts that know a product reach its
//! layer with typed access ([`ProductLayers::get`], [`ProductLayers::get_mut`]
//! and the per-product shorthands of the built products).
use ferrite_product_api::{ProductId, ProductLayer, SceneRole};

/// One registered layer.
pub struct RegisteredLayer {
    /// Products served, in registration order.
    pub products: Vec<ProductId>,
    /// Where the layer's portrayal goes in the composed scene.
    pub role: SceneRole,
    pub layer: Box<dyn ProductLayer>,
}

/// The product layers of a chart state, in registration order (the order
/// open jobs offer inputs to them, install their results and compose their
/// portrayal).
#[derive(Default)]
pub struct ProductLayers {
    entries: Vec<RegisteredLayer>,
}

impl ProductLayers {
    pub fn new() -> Self {
        Self::default()
    }
    /// Register `layer` for its products with `role`.
    pub fn register(&mut self, role: SceneRole, layer: Box<dyn ProductLayer>) {
        self.entries.push(RegisteredLayer {
            products: layer.products(),
            role,
            layer,
        });
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredLayer> {
        self.entries.iter()
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut RegisteredLayer> {
        self.entries.iter_mut()
    }
    /// Layers with scene role `role`, in registration order.
    pub fn with_role(&self, role: SceneRole) -> impl Iterator<Item = &dyn ProductLayer> {
        self.entries
            .iter()
            .filter(move |e| e.role == role)
            .map(|e| e.layer.as_ref())
    }
    /// Mutable [`Self::with_role`].
    pub fn with_role_mut(
        &mut self,
        role: SceneRole,
    ) -> impl Iterator<Item = &mut (dyn ProductLayer + 'static)> {
        self.entries
            .iter_mut()
            .filter(move |e| e.role == role)
            .map(|e| e.layer.as_mut())
    }
    /// The layer serving `product`.
    pub fn serving(&self, product: ProductId) -> Option<&dyn ProductLayer> {
        self.entries
            .iter()
            .find(|e| e.products.contains(&product))
            .map(|e| e.layer.as_ref())
    }
    /// The registered layer of type `T`.
    pub fn get<T: ProductLayer>(&self) -> Option<&T> {
        self.entries
            .iter()
            .find_map(|e| e.layer.as_any().downcast_ref::<T>())
    }
    /// Mutable [`Self::get`].
    pub fn get_mut<T: ProductLayer>(&mut self) -> Option<&mut T> {
        self.entries
            .iter_mut()
            .find_map(|e| e.layer.as_any_mut().downcast_mut::<T>())
    }
    /// Products served by the registered layers, in registration order.
    pub fn products(&self) -> Vec<ProductId> {
        self.entries
            .iter()
            .flat_map(|e| e.products.iter().copied())
            .collect()
    }
}

/// Typed access to the layers of the products built into this engine; each
/// panics when the host did not register the product's layer (a host
/// error: [`crate::products`] registers them).
macro_rules! typed_layer {
    ($feature:literal, $get:ident, $get_mut:ident, $ty:ty, $name:literal) => {
        #[cfg(feature = $feature)]
        impl ProductLayers {
            #[doc = concat!("The ", $name, " layer.")]
            pub fn $get(&self) -> &$ty {
                self.get::<$ty>()
                    .expect(concat!("The ", $name, " layer is not registered"))
            }
            #[doc = concat!("Mutable [`Self::", stringify!($get), "`].")]
            pub fn $get_mut(&mut self) -> &mut $ty {
                self.get_mut::<$ty>()
                    .expect(concat!("The ", $name, " layer is not registered"))
            }
        }
    };
}

typed_layer!(
    "s101",
    s101,
    s101_mut,
    ferrite_s101::layer::S101Layer,
    "S-101"
);
typed_layer!(
    "s102-pure",
    s102,
    s102_mut,
    ferrite_s102::layer::S102Layer,
    "S-102"
);
typed_layer!(
    "s421",
    routes,
    routes_mut,
    ferrite_s421::layer::S421Layer,
    "S-421"
);

impl ProductLayers {
    /// The portrayal catalogue scenes are drawn with (palette, symbols): the
    /// first registered layer's that has one installed (the S-101 base
    /// chart's when S-101 is built).
    pub fn scene_catalogue(&self) -> Option<&ferrite_portrayal_catalog::BoundPortrayalCatalogue> {
        self.entries
            .iter()
            .find_map(|e| e.layer.portrayal_catalogue())
    }
    /// The GML overlay layer (S-124, S-129 and any registered GML product).
    pub fn overlays(&self) -> Option<&crate::gml_overlay::GmlOverlays> {
        self.get()
    }
    /// Mutable [`Self::overlays`].
    pub fn overlays_mut(&mut self) -> Option<&mut crate::gml_overlay::GmlOverlays> {
        self.get_mut()
    }
}
