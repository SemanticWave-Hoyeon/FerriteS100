//! Hit-testing: build the rendered-symbol list off-thread, poll for
//! completion, and pick symbols near a screen point. The build runs on
//! a worker thread so a viewport rebuild doesn't stall the main loop.

use std::sync::mpsc;

use crate::{ChartApp, RenderedSymbol};

impl ChartApp {
    /// Build rendered symbols list for hit testing
    /// Build rendered symbols asynchronously on a background thread.
    /// Results are polled via `poll_hit_test`.
    pub(crate) fn build_rendered_symbols(&mut self) {
        // Collect point data needed for building symbols
        let points: Vec<_> = self
            .render_context
            .get_sorted_instructions()
            .iter()
            .filter_map(|instr| {
                if let ferrite_render::DrawingInstruction::Point(point) = instr {
                    Some((
                        point.symbol_ref.clone(),
                        point.feature_id.unwrap_or(0),
                        point.position,
                        point.priority.0,
                        point.cell_index,
                    ))
                } else {
                    None
                }
            })
            .collect();

        let scaler = self.render_context.scaler.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_hit_test = Some(rx);

        std::thread::spawn(move || {
            let symbols: Vec<RenderedSymbol> = points
                .into_iter()
                .map(|(symbol_ref, feature_id, position, priority, cell_index)| {
                    let screen = scaler.world_to_screen(position);
                    RenderedSymbol {
                        symbol_ref,
                        feature_id,
                        screen_x: screen.x,
                        screen_y: screen.y,
                        world_x: position.x,
                        world_y: position.y,
                        priority,
                        cell_index,
                    }
                })
                .collect();
            let _ = tx.send(symbols);
        });
    }

    /// Poll for completed async hit-test build. Call this each frame.
    pub(crate) fn poll_hit_test(&mut self) {
        if let Some(rx) = &self.pending_hit_test {
            if let Ok(symbols) = rx.try_recv() {
                self.rendered_symbols = symbols;
                self.pending_hit_test = None;
            }
        }
    }

    /// Find symbols near the click position
    /// Sorted by priority (highest first = topmost visible), then by distance (closest first)
    pub(crate) fn find_symbols_at(&self, x: f64, y: f64, radius: f32) -> Vec<&RenderedSymbol> {
        let mut nearby: Vec<_> = self
            .rendered_symbols
            .iter()
            .filter_map(|s| {
                let dx = s.screen_x - x as f32;
                let dy = s.screen_y - y as f32;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq <= radius * radius {
                    Some((s, dist_sq))
                } else {
                    None
                }
            })
            .collect();

        // Sort by priority (highest first = topmost), then by distance (closest first)
        nearby.sort_by(|a, b| {
            // First compare by priority (higher priority = drawn on top)
            match b.0.priority.cmp(&a.0.priority) {
                std::cmp::Ordering::Equal => {
                    // Same priority: prefer closer symbol
                    a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                }
                other => other,
            }
        });

        nearby.into_iter().map(|(s, _)| s).collect()
    }
}
