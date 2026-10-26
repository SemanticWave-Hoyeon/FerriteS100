//! Optional bounded display-chart-pass GPU timestamps; not CPU/present/FPS timing.
use std::ffi::OsStr;
pub const MAX_FRAMES: usize = 64;
const QUERY_COUNT: u32 = (MAX_FRAMES * 2) as u32;
const BYTES: u64 = QUERY_COUNT as u64 * 8;
pub(crate) fn requested(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameKey {
    pub camera: Option<[u64; 16]>,
    pub source_revision: Option<u64>,
    pub surface_size: [u32; 2],
    pub view_affine_bits: [u32; 3],
}
#[derive(Clone, Copy)]
pub(crate) struct Ticket {
    index: usize,
    serial: u64,
    key: FrameKey,
}
#[derive(Default)]
struct Ledger {
    rows: Vec<FrameKey>,
    pending: Option<Ticket>,
    attempts: u64,
    dropped: u64,
}
impl Ledger {
    fn reserve(&mut self, key: FrameKey) -> Option<Ticket> {
        self.attempts = self.attempts.saturating_add(1);
        if self.pending.is_some() || self.rows.len() == MAX_FRAMES {
            self.dropped = self.dropped.saturating_add(1);
            return None;
        }
        let ticket = Ticket {
            index: self.rows.len(),
            serial: self.attempts,
            key,
        };
        self.pending = Some(ticket);
        Some(ticket)
    }
    fn commit(&mut self, ticket: Ticket) -> bool {
        if self.pending.is_some_and(|p| {
            p.serial == ticket.serial && p.index == ticket.index && p.key == ticket.key
        }) {
            self.rows.push(ticket.key);
            self.pending = None;
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            false
        }
    }
}
pub(crate) struct Batch {
    query: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    ledger: Ledger,
    period_ns: f32,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_format: wgpu::TextureFormat,
}
impl Batch {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> Result<Self, String> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return Err("device_timestamp_feature_not_enabled".into());
        }
        let period_ns = queue.get_timestamp_period();
        if !period_ns.is_finite() || period_ns <= 0. {
            return Err("invalid_timestamp_period".into());
        }
        Ok(Self {
            query: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("diagnostic-chart-batch"),
                ty: wgpu::QueryType::Timestamp,
                count: QUERY_COUNT,
            }),
            resolve: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("diagnostic-timestamp-resolve"),
                size: BYTES,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            readback: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("diagnostic-timestamp-readback"),
                size: BYTES,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
            ledger: Ledger {
                rows: Vec::with_capacity(MAX_FRAMES),
                ..Default::default()
            },
            period_ns,
            device: device.clone(),
            queue: queue.clone(),
            surface_format: format,
        })
    }
    pub fn matches_device_surface(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> bool {
        self.device == *device && self.queue == *queue && self.surface_format == format
    }
    pub fn drop_frame(&mut self) {
        self.ledger.dropped = self.ledger.dropped.saturating_add(1);
    }
    pub fn reserve(&mut self, key: FrameKey) -> Option<Ticket> {
        self.ledger.reserve(key)
    }
    pub fn writes(&self, ticket: Ticket) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.query,
            beginning_of_pass_write_index: Some((ticket.index * 2) as u32),
            end_of_pass_write_index: Some((ticket.index * 2 + 1) as u32),
        }
    }
    pub fn commit(&mut self, ticket: Ticket) {
        self.ledger.commit(ticket);
    }
    /// Explicit AFTER-window audit. The only poll/wait/readback added by this module.
    /// Whole batch ownership prevents query/buffer reuse across devices or mapping.
    pub fn read_after_batch(self, device: &wgpu::Device) -> Result<serde_json::Value, String> {
        if self.ledger.rows.len() != MAX_FRAMES || self.ledger.pending.is_some() {
            return Err("timestamp_batch_incomplete_no_readback".into());
        }
        if self.device != *device {
            return Err("timestamp_device_owner_changed".into());
        }
        // Audit-only: resolve after the measured window, in a separate submission.
        // The final sampled pass no longer shares its encoder with query resolution.
        // This does not guarantee that a backend records empty stage timestamps.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("diagnostic-timestamp-post-window-resolve"),
            });
        encoder.resolve_query_set(&self.query, 0..QUERY_COUNT, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, BYTES);
        self.queue.submit(std::iter::once(encoder.finish()));
        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        // Audit-only bounded host loop. No wait or poll in measured display frames.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let _ = device.poll(wgpu::Maintain::Poll);
            match rx.recv_timeout(std::time::Duration::from_millis(2)) {
                Ok(result) => {
                    result.map_err(|e| e.to_string())?;
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if std::time::Instant::now() < deadline => {}
                Err(error) => {
                    self.readback.unmap();
                    return Err(format!("timestamp_readback_timeout_or_disconnect:{error}"));
                }
            }
        }
        let data = slice.get_mapped_range();
        let mut rows = Vec::with_capacity(MAX_FRAMES);
        for (i, key) in self.ledger.rows.iter().enumerate() {
            let begin = u64::from_ne_bytes(
                data[i * 16..i * 16 + 8]
                    .try_into()
                    .map_err(|_| "timestamp bytes")?,
            );
            let end = u64::from_ne_bytes(
                data[i * 16 + 8..i * 16 + 16]
                    .try_into()
                    .map_err(|_| "timestamp bytes")?,
            );
            rows.push(serde_json::json!({"sample_index":i,"camera_bits":key.camera,"source_revision":key.source_revision,"surface_size":key.surface_size,"view_affine_bits":key.view_affine_bits,"begin_ticks":begin,"end_ticks":end,"gpu_chart_ms":duration_ms(begin,end,self.period_ns),"invalid_reason":invalid_reason(begin,end,self.period_ns)}));
        }
        drop(data);
        self.readback.unmap();
        Ok(
            serde_json::json!({"schema":2,"scope":"actual_display_chart_pass_stage_timestamps","clock_method":"render_pass_vertex_fragment_boundaries","resolve_submission_after_measured_window":true,"valid_samples":rows.iter().filter(|r| !r["gpu_chart_ms"].is_null()).count(),"unavailable_samples":rows.iter().filter(|r| r["gpu_chart_ms"].is_null()).count(),"zero_endpoint_policy":"raw ticks retained; duration unavailable, not evidence of zero GPU work","timestamp_supported":true,"surface_format":format!("{:?}",self.surface_format),"period_ns":self.period_ns,"attempts":self.ledger.attempts,"completed_samples":self.ledger.rows.len(),"dropped":self.ledger.dropped,"rows":rows,"extra_queue_submits":1,"resolve_copy_batches":1,"includes_cpu_host_or_surface_present":false,"readback_wait_outside_measurement":true}),
        )
    }
}
fn duration_ms(begin: u64, end: u64, period: f32) -> Option<f64> {
    // Zero endpoints can be an unwritten/unsupported backend stage. Preserve the
    // raw result, but never convert this ambiguity into measured zero GPU work.
    if begin == 0 || end == 0 {
        return None;
    }
    let ticks = end.checked_sub(begin)?;
    if !period.is_finite() || period <= 0. {
        return None;
    }
    let value = ticks as f64 * f64::from(period) / 1_000_000.;
    (value.is_finite() && (0. ..=3_600_000.).contains(&value)).then_some(value)
}
fn invalid_reason(begin: u64, end: u64, period: f32) -> Option<&'static str> {
    if begin == 0 || end == 0 {
        Some("timestamp_endpoint_zero_or_not_written")
    } else if end < begin {
        Some("counter_wrap_or_invalid_order")
    } else if duration_ms(begin, end, period).is_none() {
        Some("invalid_timestamp_period_or_duration")
    } else {
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> FrameKey {
        FrameKey {
            camera: None,
            source_revision: Some(1),
            surface_size: [800, 600],
            view_affine_bits: [0, 0, 1_f32.to_bits()],
        }
    }
    #[test]
    fn exact_flag_and_invalid_gpu_ticks_do_not_become_zero() {
        assert!(requested(Some(OsStr::new("1"))));
        for value in [None, Some(OsStr::new("0")), Some(OsStr::new("true"))] {
            assert!(!requested(value));
        }
        assert_eq!(duration_ms(100, 200, 1.), Some(0.0001));
        assert_eq!(duration_ms(200, 100, 1.), None);
        assert_eq!(duration_ms(1, 2, f32::NAN), None);
    }
    #[test]
    fn zero_endpoints_remain_unavailable_and_equal_nonzero_ticks_remain_valid() {
        for (begin, end) in [(0, 0), (0, 42), (42, 0)] {
            assert_eq!(duration_ms(begin, end, 1.), None);
            assert_eq!(
                invalid_reason(begin, end, 1.),
                Some("timestamp_endpoint_zero_or_not_written")
            );
        }
        assert_eq!(duration_ms(42, 42, 1.), Some(0.));
        assert_eq!(invalid_reason(42, 42, 1.), None);
        assert_eq!(
            invalid_reason(42, 41, 1.),
            Some("counter_wrap_or_invalid_order")
        );
        assert_eq!(
            invalid_reason(41, 42, f32::NAN),
            Some("invalid_timestamp_period_or_duration")
        );
    }
    #[test]
    fn bounded_ledger_rejects_duplicate_stale_and_overflow() {
        let mut ledger = Ledger::default();
        let t = ledger.reserve(key()).unwrap();
        assert!(ledger.reserve(key()).is_none());
        let stale = Ticket {
            key: FrameKey {
                source_revision: Some(2),
                ..key()
            },
            ..t
        };
        assert!(!ledger.commit(stale));
        assert!(ledger.commit(t));
        assert!(!ledger.commit(t));
        for _ in 1..MAX_FRAMES {
            let t = ledger.reserve(key()).unwrap();
            assert!(ledger.commit(t));
        }
        assert!(ledger.reserve(key()).is_none());
        assert_eq!(ledger.rows.len(), MAX_FRAMES);
        assert_eq!(ledger.dropped, 4);
    }
}
