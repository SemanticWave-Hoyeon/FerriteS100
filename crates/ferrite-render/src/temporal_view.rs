//! Product-independent viewing-time selection shared by applications and renderers.
use crate::DisplaySettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TemporalViewMode {
    #[default]
    Live,
    Date,
    Instant,
    All,
}
impl TemporalViewMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "Live clock",
            Self::Date => "Fixed date",
            Self::Instant => "Fixed date and time",
            Self::All => "Show all dates",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalView {
    pub mode: TemporalViewMode,
    pub date: String,
    pub instant: String,
    pub source_offset: String,
}
impl Default for TemporalView {
    fn default() -> Self {
        let now = chrono::Utc::now();
        Self {
            mode: TemporalViewMode::Live,
            date: now.format("%Y-%m-%d").to_string(),
            instant: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            source_offset: "Z".into(),
        }
    }
}
impl TemporalView {
    pub fn from_settings(settings: &DisplaySettings) -> Self {
        let defaults = Self::default();
        let mode = if !settings.date_dependent {
            TemporalViewMode::All
        } else if settings.current_datetime.is_some() {
            TemporalViewMode::Instant
        } else if settings.current_date.is_some() {
            TemporalViewMode::Date
        } else {
            TemporalViewMode::Live
        };
        let date = settings.current_date.clone().unwrap_or(defaults.date);
        let instant = settings
            .current_datetime
            .clone()
            .unwrap_or(defaults.instant);
        let source_offset = chrono::FixedOffset::east_opt(settings.local_time_offset_seconds)
            .map(|o| {
                if o.local_minus_utc() == 0 {
                    "Z".into()
                } else {
                    o.to_string()
                }
            })
            .unwrap_or_else(|| settings.local_time_offset_seconds.to_string());
        Self {
            mode,
            date,
            instant,
            source_offset,
        }
    }
    /// Validate the entire proposed selection before changing any active settings.
    pub fn apply(&self, settings: &mut DisplaySettings) -> Result<(), String> {
        let offset = ferrite_kernel::parse_local_time_offset(self.source_offset.trim())
            .map_err(|e| format!("Source offset: {e}"))?;
        match self.mode {
            TemporalViewMode::Date => {
                ferrite_kernel::parse_viewing_date(self.date.trim())
                    .map_err(|e| format!("Date: {e}"))?;
            }
            TemporalViewMode::Instant => {
                ferrite_kernel::parse_viewing_instant(self.instant.trim())
                    .map_err(|e| format!("Date and time: {e}"))?;
            }
            _ => {}
        }
        settings.date_dependent = self.mode != TemporalViewMode::All;
        settings.current_date =
            (self.mode == TemporalViewMode::Date).then(|| self.date.trim().to_string());
        settings.current_datetime =
            (self.mode == TemporalViewMode::Instant).then(|| self.instant.trim().to_string());
        settings.local_time_offset_seconds = offset.local_minus_utc();
        Ok(())
    }
    pub fn validation_error(&self) -> Option<String> {
        self.apply(&mut DisplaySettings::default()).err()
    }
    pub fn summary(&self) -> String {
        match self.mode {
            TemporalViewMode::Live => "Time: Live".into(),
            TemporalViewMode::Date => format!("Time: {}", self.date.trim()),
            TemporalViewMode::Instant => format!("Time: {}", self.instant.trim()),
            TemporalViewMode::All => "Time: All dates".into(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_edits_preserve_every_active_temporal_setting() {
        let mut settings = DisplaySettings::default();
        settings.current_datetime = Some("20261004T010000Z".into());
        settings.local_time_offset_seconds = 32400;
        let old = TemporalView::from_settings(&settings);
        let mut draft = old.clone();
        draft.instant = "2026-10-04T12:00:00".into();
        assert!(draft.apply(&mut settings).is_err());
        assert_eq!(TemporalView::from_settings(&settings), old);
        draft.instant = "2026-10-04T12:00:00+09:00".into();
        draft.source_offset = "+25:00".into();
        assert!(draft.apply(&mut settings).is_err());
        assert_eq!(TemporalView::from_settings(&settings), old);
    }
    #[test]
    fn transitions_clear_old_selectors_and_keep_source_offset_independent() {
        let mut settings = DisplaySettings::default();
        let mut view = TemporalView {
            mode: TemporalViewMode::Instant,
            instant: "2026-10-04T12:00:00+09:00".into(),
            source_offset: "Z".into(),
            ..Default::default()
        };
        view.apply(&mut settings).unwrap();
        assert_eq!(settings.local_time_offset_seconds, 0);
        view.mode = TemporalViewMode::Date;
        view.date = "2026-10-04".into();
        view.source_offset = "+09:00".into();
        view.apply(&mut settings).unwrap();
        assert!(settings.current_datetime.is_none());
        assert_eq!(settings.local_time_offset_seconds, 32400);
        view.mode = TemporalViewMode::All;
        view.apply(&mut settings).unwrap();
        assert!(!settings.date_dependent);
        assert!(settings.current_date.is_none());
        view.mode = TemporalViewMode::Live;
        view.apply(&mut settings).unwrap();
        assert!(settings.date_dependent);
        assert!(settings.current_date.is_none() && settings.current_datetime.is_none());
    }
    #[test]
    fn cli_selectors_round_trip_without_implicit_timezone_changes() {
        let mut settings = DisplaySettings::default();
        settings.current_date = Some("2026-10-04".into());
        settings.local_time_offset_seconds = -12600;
        let view = TemporalView::from_settings(&settings);
        assert_eq!(view.source_offset, "-03:30");
        assert_eq!(view.mode, TemporalViewMode::Date);
        let mut restored = DisplaySettings::default();
        view.apply(&mut restored).unwrap();
        assert_eq!(restored.current_date, settings.current_date);
        assert_eq!(
            restored.local_time_offset_seconds,
            settings.local_time_offset_seconds
        );
        restored.date_dependent = false;
        assert_eq!(
            TemporalView::from_settings(&restored).mode,
            TemporalViewMode::All
        );
    }
}
