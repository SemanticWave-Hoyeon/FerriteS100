//! Data model of the S-124 warning list panel (S-124 11.1.1): the rows the
//! product publishes and the values its requests carry. UI crates draw it.
use crate::navwarn::{self, SortOrder, Status, UserClassification};
use crate::overlay::{cancel_reason, timing_label, S124Overlay};
use chrono::Utc;
use ferrite_s100_gml::overlay::{format_time, OverlayCore};
use ferrite_s100_gml::product::GmlOverlayProduct;

/// User relevance classification (S-124 11.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavwarnClassification {
    #[default]
    OnChart,
    OffChart,
    Information,
}

impl NavwarnClassification {
    pub const ALL: [Self; 3] = [Self::OnChart, Self::OffChart, Self::Information];
    pub fn label(self) -> &'static str {
        match self {
            Self::OnChart => "On chart",
            Self::OffChart => "Off chart",
            Self::Information => "Information",
        }
    }
}

/// Listing order (S-124 11.1.1 b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavwarnSort {
    #[default]
    Received,
    IssueTime,
    WarningType,
    ProducerSeries,
    Classification,
}

impl NavwarnSort {
    pub const ALL: [Self; 5] = [
        Self::Received,
        Self::IssueTime,
        Self::WarningType,
        Self::ProducerSeries,
        Self::Classification,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Received => "Received",
            Self::IssueTime => "Issue time",
            Self::WarningType => "Warning type",
            Self::ProducerSeries => "Producer / series",
            Self::Classification => "Classification",
        }
    }
}

/// One NavwarnPart of a listed message.
#[derive(Debug, Clone, Default)]
pub struct NavwarnPartRow {
    pub id: String,
    /// Abbreviated warningInformation (11.1.1 g).
    pub summary: String,
    pub timing: String,
    pub has_geometry: bool,
}

/// One listed S-124 dataset (message, bulletin or cancellation).
#[derive(Debug, Clone, Default)]
pub struct NavwarnRow {
    /// Host dataset identity.
    pub dataset: u32,
    pub caption: String,
    pub status: String,
    pub cancelled: bool,
    pub imported: bool,
    /// New-message indication (11.1.1 c).
    pub new: bool,
    pub portrayed: bool,
    pub received: String,
    pub issued: String,
    pub warning_type: String,
    pub producer_series: String,
    pub general_area: String,
    pub classification: NavwarnClassification,
    pub parts: Vec<NavwarnPartRow>,
    pub source: String,
}

impl From<NavwarnSort> for SortOrder {
    fn from(sort: NavwarnSort) -> Self {
        match sort {
            NavwarnSort::Received => SortOrder::Received,
            NavwarnSort::IssueTime => SortOrder::IssueTime,
            NavwarnSort::WarningType => SortOrder::WarningType,
            NavwarnSort::ProducerSeries => SortOrder::ProducerSeries,
            NavwarnSort::Classification => SortOrder::UserClassification,
        }
    }
}

impl From<NavwarnClassification> for UserClassification {
    fn from(class: NavwarnClassification) -> Self {
        match class {
            NavwarnClassification::OnChart => UserClassification::OnChart,
            NavwarnClassification::OffChart => UserClassification::OffChart,
            NavwarnClassification::Information => UserClassification::Information,
        }
    }
}

impl From<UserClassification> for NavwarnClassification {
    fn from(class: UserClassification) -> Self {
        match class {
            UserClassification::OnChart => NavwarnClassification::OnChart,
            UserClassification::OffChart => NavwarnClassification::OffChart,
            UserClassification::Information => NavwarnClassification::Information,
        }
    }
}

impl S124Overlay {
    /// Rows of the warning list in the selected order, the in-force bulletin
    /// missing-message indications (9.2.2) and the number of new messages.
    pub fn navwarn_rows(
        &mut self,
        core: &mut OverlayCore,
    ) -> (Vec<NavwarnRow>, Vec<String>, usize) {
        self.evaluate(core, false);
        let registry = self.registry(core).clone();
        let mut rows = Vec::new();
        let mut new_count = 0;
        let clock = Utc::now();
        for index in registry.sorted(self.sort, &self.user) {
            let m = &registry.messages[index];
            let Some(entry) = self
                .registry_ids
                .get(index)
                .and_then(|id| core.dataset(*id))
            else {
                continue;
            };
            let class = self.user.classification_of(m);
            let meets = class != UserClassification::Information;
            let new = registry.is_new_at(index, &self.user, meets, clock);
            new_count += new as usize;
            rows.push(NavwarnRow {
                dataset: entry.id,
                caption: m.caption(),
                status: match &m.status {
                    Status::Cancelled(reason) => {
                        format!("Cancelled ({})", cancel_reason(reason, &registry))
                    }
                    s => s.label().to_owned(),
                },
                cancelled: m.status.is_cancelled(),
                imported: m.imported,
                new,
                portrayed: entry.visible
                    && registry.dataset_portrayed(index, &self.user, &self.policy),
                received: format_time(m.received),
                issued: m
                    .publication_time
                    .map(format_time)
                    .unwrap_or_else(|| "-".into()),
                warning_type: m
                    .warning_type
                    .map(|t| {
                        navwarn::warning_type_label(t)
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("type {t}"))
                    })
                    .or_else(|| m.navwarn_type_general.clone())
                    .unwrap_or_default(),
                producer_series: m.series().map(|s| s.to_string()).unwrap_or_default(),
                general_area: m
                    .general_area
                    .iter()
                    .chain(&m.locality)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                classification: match class {
                    UserClassification::OnChart => NavwarnClassification::OnChart,
                    UserClassification::OffChart => NavwarnClassification::OffChart,
                    UserClassification::Information => NavwarnClassification::Information,
                },
                parts: m
                    .parts
                    .iter()
                    .map(|p| NavwarnPartRow {
                        id: p.id.clone(),
                        summary: p.abbreviated(120),
                        timing: timing_label(p.timing),
                        has_geometry: p.has_geometry,
                    })
                    .collect(),
                source: format!(
                    "{} · {}",
                    entry
                        .dataset
                        .source
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    entry.provenance.label()
                ),
            });
        }
        let mut missing = Vec::new();
        for b in registry.missing_indications() {
            let ids: Vec<String> = b
                .missing
                .iter()
                .map(|m| {
                    if m.imported_copy.is_some() {
                        format!("{} (imported copy only)", m.id)
                    } else {
                        m.id.to_string()
                    }
                })
                .collect();
            missing.push(format!(
                "In-force bulletin {} lists warnings not held: {}",
                b.series,
                ids.join(", ")
            ));
        }
        (rows, missing, new_count)
    }
}
