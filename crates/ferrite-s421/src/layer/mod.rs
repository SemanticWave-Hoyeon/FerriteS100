//! The engine-facing S-421 product layer (`docs/ENGINE_DESIGN.md`,
//! "제품 모듈화", step 3), built for every target.
//!
//! * [`input`]: route capture and discovery over a data source (moved from
//!   the engine's `s421_dataset_input`).
//! * [`S421`]: the product adapter (S-421 GML namespace in the head of an
//!   XML/GML file).
//! * [`S421Layer`]: the opened route datasets. It keeps the captured
//!   original bytes, reads them on demand for coverage, feature reports and
//!   portrayal, and portrays the visible routes as the route overlay
//!   renderer extension (`ferrite_product_api::route_overlay`, drawn by
//!   `ferrite_gpu::PreparedNativeRouteOverlay::from_extension`).
//! * [`overlay`]: routes as overlay inputs, shared with the viewer's route
//!   editor (`crate::editor`).
use crate::route::Route;
use crate::s421::{self, ImportedDataset, Profile};
use ferrite_product_api::route_overlay::{self, NativeRouteWorldSamples};
use ferrite_product_api::{
    DatasetInput, Encoding, FeaturePick, GeoBounds, Opened, PanelDescriptor, PortrayalRequest,
    ProductAdapter, ProductId, ProductInfo, ProductLayer, Recognised, SceneContribution, SceneRole,
    WorldPoint,
};
use std::any::Any;
use std::path::Path;
use std::sync::{Arc, OnceLock};

pub mod input;
pub mod overlay;

use input::CapturedRouteInput;

/// Product identifier.
pub const PRODUCT: ProductId = ProductId("S-421");

/// The route panel (data model [`crate::ui::UiData`], events
/// [`crate::ui::UiEvent`]).
pub const ROUTE_PANEL: PanelDescriptor = PanelDescriptor {
    id: "S-421/routes",
    title: "S-421 Routes",
};

/// The S-421 product adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct S421;

impl S421 {
    /// Whether the file name can hold a route: `.gml`/`.xml` (not an
    /// AppleDouble companion), not an exchange set catalogue.
    pub fn is_route_name(name: &Path) -> bool {
        (input::is_dataset_file(name, "gml") || input::is_dataset_file(name, "xml"))
            && !name
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("CATALOG.XML"))
    }
}

impl ProductAdapter for S421 {
    fn info(&self) -> ProductInfo {
        ProductInfo {
            id: PRODUCT,
            title: "Route Plan",
            encoding: Encoding::Gml,
            role: SceneRole::Route,
        }
    }
    /// A route name whose head names S-421 (`S421`/`S-421`). This is a cheap
    /// pre-check; [`input::capture_bytes`] decides from the root namespace.
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised> {
        if !Self::is_route_name(name) {
            return None;
        }
        let head = String::from_utf8_lossy(head);
        (head.contains("S421") || head.contains("S-421")).then_some(Recognised::Dataset)
    }
    fn panels(&self) -> &'static [PanelDescriptor] {
        std::slice::from_ref(&ROUTE_PANEL)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Key of a picked route element in [`ProductLayer::describe`] and
/// [`ProductLayer::highlight`]: route ids are those of the overlay paths
/// of the current revision (1-based, in dataset order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKey {
    Waypoint {
        route_id: u32,
        waypoint_id: u32,
    },
    /// The leg arriving at waypoint `to`.
    Leg {
        route_id: u32,
        to: u32,
    },
}

impl RouteKey {
    /// Waypoints are positive keys, legs negative.
    pub fn encode(self) -> i64 {
        match self {
            Self::Waypoint {
                route_id,
                waypoint_id,
            } => ((route_id as i64) << 32) | waypoint_id as i64,
            Self::Leg { route_id, to } => -(((route_id as i64) << 32) | to as i64),
        }
    }
    pub fn decode(key: i64) -> Option<Self> {
        let leg = key < 0;
        let value = key.checked_abs()?;
        let route_id = u32::try_from(value >> 32).ok()?;
        let id = (value & 0xFFFF_FFFF) as u32;
        Some(if leg {
            Self::Leg { route_id, to: id }
        } else {
            Self::Waypoint {
                route_id,
                waypoint_id: id,
            }
        })
    }
}

struct Entry {
    id: u32,
    input: CapturedRouteInput,
    visible: bool,
    parsed: OnceLock<Result<ImportedDataset, String>>,
}

impl Entry {
    fn dataset(&self) -> Option<&ImportedDataset> {
        self.parsed
            .get_or_init(|| s421::import_dataset(self.input.source_xml()))
            .as_ref()
            .ok()
    }
}

/// A route of the layer as drawn: its overlay route id, dataset and route.
struct Drawn<'a> {
    entry: &'a Entry,
    profile: Profile,
    imported: &'a s421::ImportedRoute,
    route: Route,
}

/// The opened S-421 route datasets.
#[derive(Default)]
pub struct S421Layer {
    entries: Vec<Entry>,
    next_id: u32,
    revision: u64,
    portrayed: Option<(u64, Arc<NativeRouteWorldSamples>)>,
    portray_error: Option<String>,
}

impl S421Layer {
    pub fn new() -> Self {
        Self::default()
    }
    /// The captured inputs, in opening order.
    pub fn inputs(&self) -> impl Iterator<Item = &CapturedRouteInput> + '_ {
        self.entries.iter().map(|e| &e.input)
    }
    /// Whether a route with these bytes (SHA-256) is open.
    pub fn contains(&self, sha256: &[u8; 32]) -> bool {
        self.entries.iter().any(|e| e.input.sha256() == sha256)
    }
    /// Add a captured route; the same content is opened once.
    pub fn insert(&mut self, input: CapturedRouteInput) -> Opened {
        if self.contains(input.sha256()) {
            return Opened::AlreadyOpen;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push(Entry {
            id,
            input,
            visible: true,
            parsed: OnceLock::new(),
        });
        self.changed();
        Opened::New(id)
    }
    /// Revision of the layer content (changes with every open, removal and
    /// visibility change).
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Why the last [`ProductLayer::portray`] produced nothing.
    pub fn portray_error(&self) -> Option<&str> {
        self.portray_error.as_deref()
    }
    fn changed(&mut self) {
        self.revision += 1;
        self.portrayed = None;
    }
    /// Every route of the readable datasets with its overlay route id;
    /// hidden datasets keep their ids.
    fn drawn(&self) -> Vec<(u32, Drawn<'_>)> {
        let mut out = Vec::new();
        let mut next = 0u32;
        for entry in &self.entries {
            let Some(dataset) = entry.dataset() else {
                continue;
            };
            for imported in &dataset.routes {
                next += 1;
                let mut route = imported.route.clone();
                route.id = next;
                out.push((
                    next,
                    Drawn {
                        entry,
                        profile: dataset.profile,
                        imported,
                        route,
                    },
                ));
            }
        }
        out
    }
    fn samples(&mut self) -> Result<Arc<NativeRouteWorldSamples>, String> {
        if let Some((revision, samples)) = &self.portrayed {
            if *revision == self.revision {
                return Ok(samples.clone());
            }
        }
        let drawn = self.drawn();
        let mut paths = Vec::new();
        let mut declarations = Vec::new();
        for (_, d) in drawn.iter().filter(|(_, d)| d.entry.visible) {
            paths.push(overlay::path(&d.route));
            overlay::imported_route_legs(&d.route, d.profile, d.imported, &mut declarations)?;
        }
        let samples = NativeRouteWorldSamples::prepare(&paths, &declarations, self.revision)?;
        self.portrayed = Some((self.revision, samples.clone()));
        Ok(samples)
    }
}

impl ProductLayer for S421Layer {
    fn products(&self) -> Vec<ProductId> {
        vec![PRODUCT]
    }
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Capture a route from its bytes (S-421 root namespace and `Dataset`
    /// root, bounded XML); the content is read on demand.
    fn open(&mut self, input: DatasetInput<'_>) -> anyhow::Result<Opened> {
        let captured = input::capture_bytes(input.name.to_path_buf(), input.bytes.to_vec())?
            .ok_or_else(|| anyhow::anyhow!("Not an S-421 route: {}", input.name.display()))?;
        Ok(self.insert(captured))
    }
    fn remove(&mut self, id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        let removed = self.entries.len() != before;
        if removed {
            self.changed();
        }
        removed
    }
    fn set_visible(&mut self, id: u32, visible: bool) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        if entry.visible != visible {
            entry.visible = visible;
            self.changed();
        }
        true
    }
    /// The visible routes as one route overlay renderer extension (no
    /// drawing instructions). None without datasets or when the routes
    /// exceed the overlay budgets ([`S421Layer::portray_error`]).
    fn portray(&mut self, _request: &PortrayalRequest<'_>) -> Option<SceneContribution> {
        if self.entries.is_empty() {
            return None;
        }
        match self.samples() {
            Ok(samples) => {
                self.portray_error = None;
                Some(SceneContribution {
                    revision: self.revision,
                    instructions: Arc::new(Vec::new()),
                    extensions: vec![route_overlay::extension(samples)],
                })
            }
            Err(error) => {
                self.portray_error = Some(error);
                None
            }
        }
    }
    /// Extent of the waypoints of the visible routes.
    fn coverage(&self) -> Option<GeoBounds> {
        let mut bounds: Option<GeoBounds> = None;
        for (_, d) in self.drawn().iter().filter(|(_, d)| d.entry.visible) {
            for p in &d.route.waypoints {
                let b = bounds.get_or_insert(GeoBounds::new(p.lon, p.lat, p.lon, p.lat));
                b.min_x = b.min_x.min(p.lon);
                b.min_y = b.min_y.min(p.lat);
                b.max_x = b.max_x.max(p.lon);
                b.max_y = b.max_y.max(p.lat);
            }
        }
        bounds
    }
    /// A waypoint or leg ([`RouteKey`]).
    fn describe(&mut self, key: i64) -> Option<FeaturePick> {
        let key = RouteKey::decode(key)?;
        let drawn = self.drawn();
        let route_id = match key {
            RouteKey::Waypoint { route_id, .. } | RouteKey::Leg { route_id, .. } => route_id,
        };
        let (_, d) = drawn.iter().find(|(id, _)| *id == route_id)?;
        let route_name = d
            .route
            .name
            .clone()
            .unwrap_or_else(|| d.imported.route_id.clone());
        let source = d.entry.input.path().display().to_string();
        match key {
            RouteKey::Waypoint { waypoint_id, .. } => {
                let point = d.route.waypoints.iter().find(|p| p.id == waypoint_id)?;
                let mut attributes = vec![
                    ("Route".to_owned(), route_name),
                    ("Waypoint".to_owned(), point.id.to_string()),
                    ("Position".to_owned(), point.format_dms()),
                ];
                if let Some(name) = &point.name {
                    attributes.push(("Name".into(), name.clone()));
                }
                if let Some(radius) = point.turn_radius {
                    attributes.push(("Turn radius".into(), format!("{radius} NM")));
                }
                Some(FeaturePick {
                    feature_type: "RouteWaypoint".into(),
                    primitive: "Point".into(),
                    source,
                    attributes,
                    definition: None,
                    foid: None,
                })
            }
            RouteKey::Leg { to, .. } => {
                let index = d.route.waypoints.iter().position(|p| p.id == to)?;
                let from = d.route.waypoints.get(index.checked_sub(1)?)?;
                let to_point = &d.route.waypoints[index];
                let declared = d
                    .imported
                    .legs
                    .iter()
                    .find(|leg| leg.from_waypoint_id == from.id && leg.to_waypoint_id == to);
                let mut attributes = vec![
                    ("Route".to_owned(), route_name),
                    ("From waypoint".to_owned(), from.id.to_string()),
                    ("To waypoint".to_owned(), to.to_string()),
                    (
                        "Geometry".to_owned(),
                        declared.map_or("not declared".into(), |leg| format!("{:?}", leg.geometry)),
                    ),
                ];
                if let Some(leg) = declared {
                    if let Ok(metrics) = crate::navigation::evaluate_leg(
                        [from.lon, from.lat],
                        [to_point.lon, to_point.lat],
                        leg.geometry,
                    ) {
                        attributes
                            .push(("Distance".into(), format!("{:.3} NM", metrics.distance_nm)));
                    }
                }
                Some(FeaturePick {
                    feature_type: "RouteWaypointLeg".into(),
                    primitive: "Curve".into(),
                    source,
                    attributes,
                    definition: None,
                    foid: declared.map(|leg| leg.gml_id.clone()),
                })
            }
        }
    }
    /// A leg's declared curve (its WGS84 samples) or straight segment.
    fn highlight(&mut self, key: i64) -> Vec<Vec<WorldPoint>> {
        let Some(RouteKey::Leg { route_id, to }) = RouteKey::decode(key) else {
            return Vec::new();
        };
        if let Ok(samples) = self.samples() {
            if let Some(points) = samples.samples.get(&(route_id, to)) {
                return vec![points.iter().map(|p| WorldPoint::new(p[0], p[1])).collect()];
            }
        }
        let drawn = self.drawn();
        let Some((_, d)) = drawn.iter().find(|(id, _)| *id == route_id) else {
            return Vec::new();
        };
        let points = &d.route.waypoints;
        match points.iter().position(|p| p.id == to) {
            Some(index) if index > 0 => vec![vec![
                WorldPoint::new(points[index - 1].lon, points[index - 1].lat),
                WorldPoint::new(points[index].lon, points[index].lat),
            ]],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_product_api::Provenance;

    /// A published 1.0 route with declared loxodrome legs.
    fn route_xml() -> String {
        use crate::route::Waypoint;
        let mut route = Route::with_name(0, "Layer route");
        for (id, lon, lat) in [(1, -2.5, 50.), (2, -2.3, 50.1), (3, -2.1, 50.15)] {
            let mut wp = Waypoint::new(id, lon, lat);
            wp.turn_radius = Some(0.5);
            if id > 1 {
                wp.incoming_geometry = Some(crate::s421::LegGeometry::Loxodrome);
            }
            route.add_waypoint(wp);
        }
        s421::export_published(
            &route,
            s421::PublishedExport {
                route_id: "LAYER.ROUTE",
                edition: 1,
                status: 1,
            },
        )
        .unwrap()
    }

    fn request() -> PortrayalRequest<'static> {
        PortrayalRequest {
            profile: "Day",
            viewing_time: None,
            settings: None,
        }
    }

    #[test]
    fn adapter_sniffs_route_heads_by_name_and_namespace() {
        let s421 = S421;
        assert_eq!(s421.id(), PRODUCT);
        assert_eq!(s421.info().role, SceneRole::Route);
        let head = br#"<Dataset xmlns="http://www.iho.int/S421/gml/cs0/1.0"/>"#;
        assert_eq!(
            s421.sniff(Path::new("r.gml"), head),
            Some(Recognised::Dataset)
        );
        assert_eq!(s421.sniff(Path::new("r.000"), head), None);
        assert_eq!(s421.sniff(Path::new("CATALOG.XML"), head), None);
        assert_eq!(s421.sniff(Path::new("._r.gml"), head), None);
        assert_eq!(
            s421.sniff(Path::new("fc.xml"), b"<FeatureCatalogue/>"),
            None
        );
        assert_eq!(s421.panels(), &[ROUTE_PANEL]);
    }

    #[test]
    fn layer_opens_bytes_once_and_reports_coverage_and_picks() {
        let mut layer = S421Layer::new();
        assert!(layer.portray(&request()).is_none());
        let open = |layer: &mut S421Layer, bytes: &[u8]| {
            layer.open(DatasetInput {
                name: Path::new("/m/route.gml"),
                bytes,
                provenance: Provenance::Unverified,
            })
        };
        let xml = route_xml();
        let id = match open(&mut layer, xml.as_bytes()).unwrap() {
            Opened::New(id) => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            open(&mut layer, xml.as_bytes()).unwrap(),
            Opened::AlreadyOpen
        );
        assert!(open(&mut layer, b"<FeatureCatalogue/>").is_err());
        assert_eq!(layer.inputs().count(), 1);

        let dataset = s421::import_dataset(&xml).unwrap();
        let waypoints = &dataset.routes[0].route.waypoints;
        let bounds = layer.coverage().unwrap();
        for p in waypoints {
            assert!((bounds.min_x..=bounds.max_x).contains(&p.lon));
            assert!((bounds.min_y..=bounds.max_y).contains(&p.lat));
        }

        let scene = layer.portray(&request()).unwrap();
        assert!(scene.instructions.is_empty());
        let samples = route_overlay::payload(&scene.extensions[0]).unwrap();
        assert_eq!(samples.paths().len(), 1);
        assert_eq!(samples.paths()[0].route_id, 1);
        assert_eq!(samples.paths()[0].waypoints.len(), waypoints.len());
        assert_eq!(samples.revision(), scene.revision);
        assert_eq!(samples.declarations().len(), 2);
        assert_eq!(samples.declarations()[0].source_profile, 1);
        // Cached until the content changes.
        let again = layer.portray(&request()).unwrap();
        assert!(Arc::ptr_eq(
            &samples,
            &route_overlay::payload(&again.extensions[0]).unwrap()
        ));

        let first = waypoints[0].id;
        let pick = layer
            .describe(
                RouteKey::Waypoint {
                    route_id: 1,
                    waypoint_id: first,
                }
                .encode(),
            )
            .unwrap();
        assert_eq!(pick.feature_type, "RouteWaypoint");
        assert_eq!(pick.source, "/m/route.gml");
        let second = waypoints[1].id;
        let leg = RouteKey::Leg {
            route_id: 1,
            to: second,
        };
        assert_eq!(RouteKey::decode(leg.encode()), Some(leg));
        assert_eq!(
            layer.describe(leg.encode()).unwrap().feature_type,
            "RouteWaypointLeg"
        );
        assert!(!layer.highlight(leg.encode()).is_empty());
        assert!(layer
            .describe(
                RouteKey::Waypoint {
                    route_id: 2,
                    waypoint_id: first
                }
                .encode()
            )
            .is_none());

        assert!(layer.set_visible(id, false));
        assert!(layer.coverage().is_none());
        let hidden = layer.portray(&request()).unwrap();
        assert!(route_overlay::payload(&hidden.extensions[0])
            .unwrap()
            .paths()
            .is_empty());
        assert!(layer.remove(id));
        assert!(!layer.remove(id));
        assert!(layer.is_empty());
    }
}
