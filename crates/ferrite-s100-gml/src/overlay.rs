//! Shared state of the GML overlay layer: the loaded datasets of every
//! registered GML product ([`OverlayDataset`]) and the layer-wide settings
//! ([`OverlayCore`]) product modules read and update through their
//! [`crate::product::GmlOverlayProduct`] hooks. The engine's overlay host
//! owns the core together with the registered product modules.
use crate::model::{Dataset, Issue, Product};
use crate::portrayal::{Portrayal, PortrayalInput};
use chrono::{DateTime, Utc};
use ferrite_product_api::platform::Clock;
use ferrite_product_api::{GeoBounds, Provenance, WorldPoint};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// A loaded dataset of the overlay layer.
pub struct OverlayDataset {
    /// Layer-local dataset id (also the slot of its feature keys).
    pub id: u32,
    pub dataset: Dataset,
    /// First receipt (S-124 keeps it across restarts).
    pub received: DateTime<Utc>,
    /// Reader and validation issues.
    pub issues: Vec<Issue>,
    pub provenance: Provenance,
    pub visible: bool,
    /// Portrayal input, built when first portrayed (host cache).
    pub input: Option<PortrayalInput>,
    /// XSLT result and the portrayal settings key it was made with (host
    /// cache).
    pub portrayal: Option<(u64, Portrayal)>,
}

impl OverlayDataset {
    pub fn product(&self) -> Product {
        self.dataset.product
    }
    pub fn name(&self) -> String {
        let mut name = self.dataset.display_name();
        if let Some(title) = self.dataset.identification.title() {
            if !title.is_empty() && title != name {
                name = format!("{name} · {title}");
            }
        }
        name
    }
    /// Bounds of the objects (those in `ids`, or all).
    pub fn bounds(&self, ids: Option<&HashSet<String>>) -> Option<GeoBounds> {
        let mut bounds: Option<GeoBounds> = None;
        for object in &self.dataset.objects {
            if ids.is_some_and(|ids| !ids.contains(&object.id)) {
                continue;
            }
            for c in object.geometry.iter().flat_map(|g| g.coords()) {
                let p = WorldPoint::new(c[0], c[1]);
                match &mut bounds {
                    Some(b) => b.expand(p),
                    None => bounds = Some(GeoBounds::new(c[0], c[1], c[0], c[1])),
                }
            }
        }
        bounds
    }
}

/// Layer-wide state shared by the overlay host and the product modules.
pub struct OverlayCore {
    /// Loaded datasets of every product, in load order (portrayal order).
    pub datasets: Vec<OverlayDataset>,
    /// Viewing time selected by the user (the S-101 date-dependent
    /// portrayal setting); None evaluates at the system clock.
    pub viewing_time: Option<DateTime<Utc>>,
    /// 0 when the product panels' rows must be published again.
    pub ui_revision: u64,
    /// Part chosen from a product list (dataset id, object id): portrayed
    /// even when filtered, so it can be interrogated (S-124 11.2).
    pub list_selection: Option<(u32, String)>,
    revision: u64,
    correct_pc_errata: bool,
    clock: Arc<dyn Clock>,
}

impl OverlayCore {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            datasets: Vec::new(),
            viewing_time: None,
            ui_revision: 0,
            list_selection: None,
            revision: 1,
            correct_pc_errata: false,
            clock,
        }
    }
    /// The platform clock.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.utc()
    }
    /// Instant lifecycles are evaluated at: the user's viewing time, else
    /// the system clock.
    pub fn lifecycle_time(&self) -> DateTime<Utc> {
        self.viewing_time.unwrap_or_else(|| self.now())
    }
    pub fn dataset(&self, id: u32) -> Option<&OverlayDataset> {
        self.datasets.iter().find(|d| d.id == id)
    }
    /// Portrayal revision: changes whenever the instructions must be made
    /// again.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Something visible changed: the next portrayal makes new instructions.
    pub fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    /// The "Correct known PC errata" option (off: catalogues applied
    /// literally per S-100 Part 9).
    pub fn correct_pc_errata(&self) -> bool {
        self.correct_pc_errata
    }
    /// Set the option's value (the host re-portrays, logs and lets the
    /// products persist it).
    pub fn set_correct_pc_errata(&mut self, on: bool) {
        self.correct_pc_errata = on;
    }
}

/// `YYYY-MM-DD HH:MMZ`.
pub fn format_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%MZ").to_string()
}

/// A `.gml`/`.xml` file name that may hold a GML dataset (not an exchange
/// catalogue, not an AppleDouble `._` file).
pub fn sniffable_name(path: &Path) -> bool {
    let named = |extension: &str| {
        !path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"._"))
            && path
                .extension()
                .is_some_and(|value| value.eq_ignore_ascii_case(extension))
    };
    (named("gml") || named("xml"))
        && !path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("CATALOG.XML"))
}

/// The product (of `products`) of a GML dataset from its first bytes: the
/// root `Dataset` element's namespace (`/S<code>/`), else the
/// productIdentifier.
pub fn sniff_head(head: &[u8], products: &[Product]) -> Option<Product> {
    let text = String::from_utf8_lossy(head);
    let mut reader = quick_xml::Reader::from_str(text.trim_start_matches('\u{feff}'));
    loop {
        use quick_xml::events::Event;
        match reader.read_event().ok()? {
            Event::Start(e) | Event::Empty(e) => {
                if e.local_name().as_ref() != b"Dataset" {
                    return None;
                }
                for attr in e.attributes().flatten() {
                    let value = String::from_utf8_lossy(&attr.value);
                    if attr.key.as_ref().starts_with(b"xmlns") {
                        for product in products {
                            if value.contains(&format!("/S{}/", product.code())) {
                                return Some(*product);
                            }
                        }
                    }
                }
                break;
            }
            Event::Eof => return None,
            _ => {}
        }
    }
    products
        .iter()
        .copied()
        .find(|product| text.contains(&format!("productIdentifier>{}<", product.identifier())))
}

/// [`sniff_head`] of a named dataset (at most its first 64 KiB are read).
pub fn sniff(name: &Path, head: &[u8], products: &[Product]) -> Option<Product> {
    if !sniffable_name(name) {
        return None;
    }
    sniff_head(&head[..head.len().min(64 * 1024)], products)
}
