//! Bounded host diagnostic only: no geometry, authority or rendering changes.
const MAX_ROWS: usize = 128;
#[derive(Clone, Copy, serde::Serialize)]
struct Row {
    call: u64,
    host_ns: u64,
    scale: u32,
    before: [u64; 10],
    after: [u64; 10],
}
#[derive(Default, serde::Serialize)]
pub(crate) struct Collector {
    calls: u64,
    host_ns: u64,
    slow_calls: u64,
    dropped: u64,
    rows: Vec<Row>,
    allocation_declined: bool,
}
impl Collector {
    pub(crate) fn record(&mut self, host_ns: u64, scale: u32, before: [u64; 10], after: [u64; 10]) {
        self.calls = self.calls.saturating_add(1);
        self.host_ns = self.host_ns.saturating_add(host_ns);
        if host_ns >= 16_666_667 {
            self.slow_calls = self.slow_calls.saturating_add(1);
        }
        if self.calls == 1 || host_ns >= 16_666_667 {
            if self.rows.len() == MAX_ROWS {
                self.dropped = self.dropped.saturating_add(1);
                return;
            }
            if self.allocation_declined {
                self.dropped = self.dropped.saturating_add(1);
                return;
            }
            if self.rows.is_empty()
                && (self.rows.try_reserve_exact(MAX_ROWS).is_err()
                    || self
                        .rows
                        .capacity()
                        .checked_mul(std::mem::size_of::<Row>())
                        .and_then(|n| n.checked_add(std::mem::size_of::<Self>()))
                        .is_none_or(|n| n > 32 * 1024))
            {
                self.rows = Vec::new();
                self.allocation_declined = true;
                self.dropped = self.dropped.saturating_add(1);
                return;
            }
            self.rows.push(Row {
                call: self.calls,
                host_ns,
                scale,
                before,
                after,
            });
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slow_records_bounded_fast_calls_not_perpoint_allocations() {
        let mut c = Collector::default();
        for _ in 0..300 {
            c.record(20_000_000, 1000, [0; 10], [0; 10]);
        }
        assert_eq!(c.rows.len(), 128);
        assert_eq!(c.dropped, 172);
        let capacity = c.rows.capacity();
        for _ in 0..500 {
            c.record(1, 1000, [0; 10], [0; 10]);
        }
        assert_eq!(c.rows.capacity(), capacity);
        assert_eq!(c.calls, 800);
    }
}
