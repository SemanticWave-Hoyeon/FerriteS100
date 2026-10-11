//! Bounded retained diagnostics; explicit levels come from the tracing adapter.
use std::collections::VecDeque;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticLevel {
    Error,
    Warning,
    Info,
}
impl DiagnosticLevel {
    pub const fn index(self) -> usize {
        match self {
            Self::Error => 0,
            Self::Warning => 1,
            Self::Info => 2,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Warning => "Warning",
            Self::Info => "Info",
        }
    }
}
#[derive(Debug, Clone)]
pub struct DiagnosticEntry {
    pub id: u64,
    pub level: DiagnosticLevel,
    pub source: std::sync::Arc<str>,
    pub message: std::sync::Arc<str>,
    pub repeats: u64,
}
#[derive(Debug, Clone, Default)]
pub struct DiagnosticLog {
    entries: VecDeque<DiagnosticEntry>,
    bytes: usize,
    counts: [usize; 3],
    dropped: u64,
    revision: u64,
    next_id: u64,
}
impl DiagnosticLog {
    pub const MAX_ROWS: usize = 512;
    /// Retained source/message UTF-8 lengths only; not allocator/RSS/global tracing memory.
    pub const MAX_BYTES: usize = 2 * 1024 * 1024;
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &DiagnosticEntry> {
        self.entries.iter()
    }
    pub fn counts(&self) -> [usize; 3] {
        self.counts
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn push(
        &mut self,
        level: DiagnosticLevel,
        source: impl AsRef<str>,
        message: impl AsRef<str>,
    ) {
        let source = source.as_ref();
        let message = message.as_ref();
        let Some(size) = source
            .len()
            .checked_add(message.len())
            .filter(|&n| n <= Self::MAX_BYTES)
        else {
            self.dropped = self.dropped.saturating_add(1);
            self.changed();
            return;
        };
        if let Some(row) = self.entries.iter_mut().find(|r| {
            r.level == level && r.source.as_ref() == source && r.message.as_ref() == message
        }) {
            row.repeats = row.repeats.saturating_add(1);
            self.changed();
            return;
        }
        while self.entries.len() >= Self::MAX_ROWS || self.bytes + size > Self::MAX_BYTES {
            if let Some(row) = self.entries.pop_front() {
                self.bytes -= row.source.len() + row.message.len();
                self.counts[row.level.index()] -= 1;
                self.dropped = self.dropped.saturating_add(row.repeats);
            } else {
                break;
            }
        }
        let Some(next) = self.next_id.checked_add(1) else {
            self.dropped = self.dropped.saturating_add(1);
            self.changed();
            return;
        };
        self.next_id = next;
        self.entries.push_back(DiagnosticEntry {
            id: next,
            level,
            source: source.into(),
            message: message.into(),
            repeats: 1,
        });
        self.bytes += size;
        self.counts[level.index()] += 1;
        self.changed();
    }
    /// Compatibility ingress at notice assignment/on explicit Logs click, never every frame.
    /// Partial-success first line remains Info; skipped/failed details are warnings.
    pub fn push_notice(&mut self, notice: &str) {
        for line in notice.lines().filter(|l| !l.trim().is_empty()) {
            let level = if line.starts_with("Error:") || line.starts_with("ERROR ") {
                DiagnosticLevel::Error
            } else if line.starts_with("Skipped ")
                || line.starts_with("Warning:")
                || line.starts_with("Failed ")
            {
                DiagnosticLevel::Warning
            } else {
                DiagnosticLevel::Info
            };
            self.push(level, "Dataset loading", line);
        }
    }
    pub fn record_dropped(&mut self, count: u64) {
        if count > 0 {
            self.dropped = self.dropped.saturating_add(count);
            self.changed();
        }
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.counts = [0; 3];
        self.dropped = 0;
        self.changed();
    }
}
/// View stores only bounded IDs. Search allocates lowercase text only on a query/log change.
#[derive(Debug, Clone)]
pub struct DiagnosticView {
    pub query: String,
    pub levels: [bool; 3],
    revision: Option<u64>,
    last_query: String,
    last_levels: [bool; 3],
    matches: Vec<u64>,
    pub snapshot: Vec<DiagnosticEntry>,
    pub counts: [usize; 3],
    pub dropped: u64,
    pub retained_bytes: usize,
}
impl Default for DiagnosticView {
    fn default() -> Self {
        Self {
            query: String::new(),
            levels: [true; 3],
            revision: None,
            last_query: String::new(),
            last_levels: [true; 3],
            matches: Vec::new(),
            snapshot: Vec::new(),
            counts: [0; 3],
            dropped: 0,
            retained_bytes: 0,
        }
    }
}
impl DiagnosticView {
    pub fn refresh(&mut self, log: &DiagnosticLog) {
        if self.revision == Some(log.revision())
            && self.query == self.last_query
            && self.levels == self.last_levels
        {
            return;
        }
        if self.query.len() > 4096 {
            let mut end = 4096;
            while !self.query.is_char_boundary(end) {
                end -= 1;
            }
            self.query.truncate(end);
        }
        if self.revision != Some(log.revision()) {
            self.snapshot = log.entries().cloned().collect();
            self.counts = log.counts();
            self.dropped = log.dropped();
            self.retained_bytes = log.retained_bytes();
        }
        let q = self.query.to_lowercase();
        self.matches.clear();
        for row in &self.snapshot {
            if self.levels[row.level.index()]
                && (q.is_empty()
                    || row.source.to_lowercase().contains(&q)
                    || row.message.to_lowercase().contains(&q))
            {
                self.matches.push(row.id);
            }
        }
        self.revision = Some(log.revision());
        self.last_query.clone_from(&self.query);
        self.last_levels = self.levels;
    }
    pub fn includes(&self, id: u64) -> bool {
        self.matches.binary_search(&id).is_ok()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_levels_and_overflow_keep_exact_budget() {
        let mut l = DiagnosticLog::default();
        l.push(DiagnosticLevel::Warning, "/data/file", "Skipped file");
        l.push(DiagnosticLevel::Warning, "/data/file", "Skipped file");
        assert_eq!(l.counts(), [0, 1, 0]);
        assert_eq!(l.entries().next().unwrap().repeats, 2);
        for i in 0..600 {
            l.push(DiagnosticLevel::Info, "test", format!("row {i}"));
        }
        assert_eq!(l.entries().count(), 512);
        assert!(l.dropped() > 0);
        assert!(l.retained_bytes() <= DiagnosticLog::MAX_BYTES);
        let before = l.retained_bytes();
        l.push(
            DiagnosticLevel::Error,
            "test",
            "x".repeat(DiagnosticLog::MAX_BYTES),
        );
        assert_eq!(l.retained_bytes(), before);
    }
    #[test]
    fn byte_cap_eviction_and_clear() {
        let mut l = DiagnosticLog::default();
        for _ in 0..3 {
            l.push(DiagnosticLevel::Info, "a", "x".repeat(800_000));
        }
        assert_eq!(l.entries().count(), 1);
        assert_eq!(l.entries().next().unwrap().repeats, 3);
        l.push(DiagnosticLevel::Info, "b", "y".repeat(800_000));
        l.push(DiagnosticLevel::Error, "c", "z".repeat(800_000));
        assert!(l.retained_bytes() <= DiagnosticLog::MAX_BYTES);
        assert!(l.dropped() >= 3);
        l.clear();
        assert_eq!(l.counts(), [0; 3]);
        assert_eq!(l.retained_bytes(), 0);
    }
    #[test]
    fn partial_success_and_search_are_not_blanket_errors() {
        let mut l = DiagnosticLog::default();
        l.push_notice("Dataset loading complete. 17 charts available.\nSkipped HDF /data/cell.h5: bad metadata\nError: decoder failed");
        assert_eq!(l.counts(), [1, 1, 1]);
        let mut v = DiagnosticView::default();
        v.query = "CELL.H5".into();
        v.refresh(&l);
        assert_eq!(l.entries().filter(|r| v.includes(r.id)).count(), 1);
        v.levels[1] = false;
        v.refresh(&l);
        assert!(!l.entries().any(|r| v.includes(r.id)));
    }
}
