//! The panel registry (`docs/ENGINE_DESIGN.md`, "제품 모듈화", step 5):
//! the windows of the registered products, each drawing its product's panel
//! data model (rows and options defined by the product crate, published by
//! the host) and recording requests the host consumes on the next frame.
//! Which panels exist depends on the UI's product features (`s124`,
//! `s129`); hosts reach a product's panel by type ([`PanelRegistry::get`],
//! [`PanelRegistry::get_mut`]).
use ferrite_product_api::PanelDescriptor;
use std::any::Any;

/// Options several product panels share, and whether a panel changed one.
#[derive(Debug, Clone, Default)]
pub struct SharedPanelState {
    /// "Correct known PC errata" (shared by the S-124 and S-129 windows);
    /// off applies the portrayal catalogues literally per S-100 Part 9.
    pub correct_pc_errata: bool,
    /// An option of a panel (or a shared one) changed; consumed by the host.
    pub options_changed: bool,
}

/// One product's panel: its state (the product's data model and the
/// requests it records) and how it is drawn.
pub trait ProductPanel: Any + std::fmt::Debug {
    /// The panel the product publishes.
    fn descriptor(&self) -> PanelDescriptor;
    /// Whether the window is shown.
    fn is_open(&self) -> bool;
    fn set_open(&mut self, open: bool);
    /// The label of the panel's entry in the view menu.
    fn menu_label(&self) -> String {
        self.descriptor().title.to_owned()
    }
    /// Draw the window when it is open.
    fn draw(&mut self, ctx: &egui::Context, shared: &mut SharedPanelState);
    fn clone_box(&self) -> Box<dyn ProductPanel>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// The panels of the products built into this UI, in registration order.
#[derive(Debug)]
pub struct PanelRegistry {
    panels: Vec<Box<dyn ProductPanel>>,
    pub shared: SharedPanelState,
}

impl Clone for PanelRegistry {
    fn clone(&self) -> Self {
        Self {
            panels: self.panels.iter().map(|p| p.clone_box()).collect(),
            shared: self.shared.clone(),
        }
    }
}

impl Default for PanelRegistry {
    /// The panels of every product built into the UI (S-124 warning list,
    /// S-129 plan panel).
    // The pushes depend on the enabled features.
    #[allow(clippy::vec_init_then_push)]
    fn default() -> Self {
        #[allow(unused_mut)]
        let mut panels: Vec<Box<dyn ProductPanel>> = Vec::new();
        #[cfg(feature = "s124")]
        panels.push(Box::<crate::overlay_ui::NavwarnPanel>::default());
        #[cfg(feature = "s129")]
        panels.push(Box::<crate::overlay_ui::UkcPanel>::default());
        Self {
            panels,
            shared: SharedPanelState::default(),
        }
    }
}

impl PanelRegistry {
    /// No panels.
    pub fn empty() -> Self {
        Self {
            panels: Vec::new(),
            shared: SharedPanelState::default(),
        }
    }
    /// Add a panel after the registered ones.
    pub fn register(&mut self, panel: Box<dyn ProductPanel>) {
        self.panels.push(panel);
    }
    pub fn iter(&self) -> impl Iterator<Item = &dyn ProductPanel> {
        self.panels.iter().map(|p| p.as_ref())
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Box<dyn ProductPanel>> {
        self.panels.iter_mut()
    }
    /// The panel with descriptor id `id`.
    pub fn by_id(&mut self, id: &str) -> Option<&mut Box<dyn ProductPanel>> {
        self.panels.iter_mut().find(|p| p.descriptor().id == id)
    }
    /// The registered panel of type `T`.
    pub fn get<T: ProductPanel>(&self) -> Option<&T> {
        self.panels.iter().find_map(|p| p.as_any().downcast_ref())
    }
    /// Mutable [`Self::get`].
    pub fn get_mut<T: ProductPanel>(&mut self) -> Option<&mut T> {
        self.panels
            .iter_mut()
            .find_map(|p| p.as_any_mut().downcast_mut())
    }
    /// Whether a panel changed an option since the last call (host side).
    pub fn take_options_changed(&mut self) -> bool {
        std::mem::take(&mut self.shared.options_changed)
    }
    /// Draw every open panel.
    pub fn draw(&mut self, ctx: &egui::Context) {
        for panel in &mut self.panels {
            panel.draw(ctx, &mut self.shared);
        }
    }
    /// One view-menu checkbox per panel.
    pub fn menu(&mut self, ui: &mut egui::Ui) {
        for panel in &mut self.panels {
            let mut open = panel.is_open();
            if ui.checkbox(&mut open, panel.menu_label()).changed() {
                panel.set_open(open);
            }
        }
    }
}
