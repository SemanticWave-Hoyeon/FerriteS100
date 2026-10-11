//! Bounded UI warning/error capture, including worker threads and DebugOFF.
use ferrite_wgpu::diagnostics::{DiagnosticLevel, DiagnosticLog};
use std::fmt::{self, Write};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use tracing::{
    field::{Field, Visit},
    Event, Level, Subscriber,
};
use tracing_subscriber::{layer::Context, Layer};
static SHARED: OnceLock<Arc<Mutex<DiagnosticLog>>> = OnceLock::new();
static DROPPED: OnceLock<Arc<AtomicU64>> = OnceLock::new();
pub fn shared() -> Arc<Mutex<DiagnosticLog>> {
    Arc::clone(SHARED.get_or_init(|| Arc::new(Mutex::new(DiagnosticLog::default()))))
}
pub fn shared_dropped() -> Arc<AtomicU64> {
    Arc::clone(DROPPED.get_or_init(|| Arc::new(AtomicU64::new(0))))
}
pub struct DiagnosticLayer {
    log: Arc<Mutex<DiagnosticLog>>,
    dropped: Arc<AtomicU64>,
}
impl Default for DiagnosticLayer {
    fn default() -> Self {
        Self {
            log: shared(),
            dropped: shared_dropped(),
        }
    }
}
const MAX_EVENT_BYTES: usize = 8192;
#[derive(Default)]
struct BoundedVisitor {
    text: String,
    truncated: bool,
}
impl Write for BoundedVisitor {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let remaining = MAX_EVENT_BYTES.saturating_sub(self.text.len());
        let mut end = remaining.min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        self.truncated |= end < value.len();
        Ok(())
    }
}
impl Visit for BoundedVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() != "message" {
            let _ = write!(self, " {}=", field.name());
        }
        let _ = write!(self, "{value:?}");
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() != "message" {
            let _ = write!(self, " {}=", field.name());
        }
        let _ = self.write_str(value);
    }
}
impl<S: Subscriber> Layer<S> for DiagnosticLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let level = match *event.metadata().level() {
            Level::ERROR => DiagnosticLevel::Error,
            Level::WARN => DiagnosticLevel::Warning,
            _ => return,
        };
        let mut visitor = BoundedVisitor::default();
        event.record(&mut visitor);
        if visitor.truncated {
            visitor.text.push_str(" [truncated at 8192 bytes]");
        }
        // UI rendering and tracing may be reentrant; never wait on its mutex.
        let Ok(mut log) = self.log.try_lock() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let lost = self.dropped.swap(0, Ordering::Relaxed);
        if lost != 0 {
            log.record_dropped(lost);
        }
        log.push(level, event.metadata().target(), &visitor.text);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;
    #[test]
    fn warning_and_error_capture_without_info_noise_or_global_subscriber() {
        let log = Arc::new(Mutex::new(DiagnosticLog::default()));
        let dropped = Arc::new(AtomicU64::new(0));
        let subscriber = tracing_subscriber::registry().with(DiagnosticLayer {
            log: Arc::clone(&log),
            dropped,
        });
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("skipped older product");
            tracing::error!("invalid signature");
            tracing::info!("per-frame profiler");
        });
        let log = log.lock().unwrap();
        assert_eq!(log.entries().len(), 2);
        assert!(log.entries().any(|e| e.level == DiagnosticLevel::Error));
        assert!(log.entries().any(|e| e.level == DiagnosticLevel::Warning));
    }
    #[test]
    fn contended_capture_never_blocks_and_reports_loss_on_next_event() {
        let log = Arc::new(Mutex::new(DiagnosticLog::default()));
        let dropped = Arc::new(AtomicU64::new(0));
        let subscriber = tracing_subscriber::registry().with(DiagnosticLayer {
            log: Arc::clone(&log),
            dropped: Arc::clone(&dropped),
        });
        tracing::subscriber::with_default(subscriber, || {
            let guard = log.lock().unwrap();
            tracing::warn!("contended");
            assert_eq!(dropped.load(Ordering::Relaxed), 1);
            drop(guard);
            tracing::warn!("next event");
        });
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(log.lock().unwrap().dropped(), 1);
    }
    #[test]
    fn oversized_utf8_debug_fields_are_bounded_before_log_storage() {
        let mut visitor = BoundedVisitor::default();
        visitor.write_str(&"해".repeat(10000)).unwrap();
        assert!(visitor.text.len() <= MAX_EVENT_BYTES);
        assert!(visitor.truncated);
        assert!(visitor.text.is_char_boundary(visitor.text.len()));
    }
}
