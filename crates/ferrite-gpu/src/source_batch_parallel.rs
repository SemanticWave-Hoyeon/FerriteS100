//! Source-only speculation. Never grants permission or emits geometry.
use crate::shared_cell::Shared;
use crate::source_line_projection_arena::{Entry, ProjectionInput};
use ferrite_render::{Scaler, ScreenPoint, WorldPoint};
use rayon::prelude::*;
use std::cell::Cell;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
const CAP: usize = 4 * 1024 * 1024;
const CONTROL: usize = 1024;
const MIN_SELECTED_POINTS: usize = 8192;
static SCRATCH: Mutex<Vec<ScreenPoint>> = Mutex::new(Vec::new());
static POOL: OnceLock<Option<Arc<rayon::ThreadPool>>> = OnceLock::new();
#[derive(Clone, Copy, Default, serde::Serialize)]
pub(crate) struct Statistics {
    enabled: bool,
    pool_available: bool,
    census_enabled: bool,
    batches: u64,
    selected_points: u64,
    arena_points: u64,
    consumed_points: u64,
    scratch_retained_bytes: usize,
    scratch_cap: usize,
}
pub(crate) struct Cache {
    pool: Option<Arc<rayon::ThreadPool>>,
    work: Option<Shared<Statistics>>,
}
pub(crate) struct Batch<'a> {
    input: ProjectionInput<'a>,
    scaler: &'a Scaler,
    mask: &'a [bool],
    outputs: MutexGuard<'static, Vec<ScreenPoint>>,
    work: Option<Shared<Statistics>>,
    consumed: Cell<u64>,
}
impl Cache {
    pub(crate) fn enabled(&self) -> bool {
        self.pool.is_some()
    }
    pub(crate) fn new(flag: Option<&std::ffi::OsStr>) -> Self {
        let pool = if flag == Some(std::ffi::OsStr::new("1")) {
            POOL.get_or_init(|| {
                if std::thread::available_parallelism().ok()?.get() <= 2 {
                    return None;
                }
                rayon::ThreadPoolBuilder::new()
                    .num_threads(2)
                    .stack_size(512 * 1024)
                    .thread_name(|i| format!("ferrite-line-{i}"))
                    .build()
                    .ok()
                    .map(Arc::new)
            })
            .clone()
        } else {
            None
        };
        let enabled = flag == Some(std::ffi::OsStr::new("1"));
        let work = enabled.then(|| {
            Shared::new(Statistics {
                enabled,
                pool_available: pool.is_some(),
                census_enabled: std::env::var("FERRITE_SOURCE_BATCH_CENSUS")
                    .is_ok_and(|v| v == "1"),
                scratch_cap: CAP,
                ..Statistics::default()
            })
        });
        Self { pool, work }
    }
    pub(crate) fn fork_cold(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            work: self.work.as_ref().map(|w| {
                Shared::new(Statistics {
                    enabled: true,
                    pool_available: self.pool.is_some(),
                    census_enabled: w.get().census_enabled,
                    scratch_cap: CAP,
                    ..Statistics::default()
                })
            }),
        }
    }
    pub(crate) fn statistics(&self) -> Statistics {
        self.work.as_ref().map_or(
            Statistics {
                scratch_cap: CAP,
                ..Statistics::default()
            },
            |w| w.get(),
        )
    }
    pub(crate) fn prepare<'a>(
        &self,
        input: ProjectionInput<'a>,
        scaler: &'a Scaler,
        mask: &'a [bool],
        no_parent: bool,
    ) -> Option<Batch<'a>> {
        if !no_parent
            || rayon::current_thread_index().is_some()
            || !crate::source_line_projection_arena::prepared_projection_matches(scaler)
        {
            return None;
        }
        let pool = self.pool.as_ref()?;
        // Current invocation mask is used only to avoid pure speculative work.
        // Missing entries mean original fallback, never a permission grant.
        let selected = input.selected_points(mask)?;
        if selected < MIN_SELECTED_POINTS {
            return None;
        }
        let len = input.point_count();
        if len
            .checked_mul(std::mem::size_of::<ScreenPoint>())?
            .checked_add(CONTROL)?
            > CAP
        {
            return None;
        }
        let mut outputs = SCRATCH.try_lock().ok()?;
        if outputs.capacity() < len {
            let extra = len.saturating_sub(outputs.len());
            outputs.try_reserve_exact(extra).ok()?;
        }
        if outputs
            .capacity()
            .checked_mul(std::mem::size_of::<ScreenPoint>())?
            .checked_add(CONTROL)?
            > CAP
        {
            *outputs = Vec::new();
            return None;
        }
        if outputs.len() < len {
            outputs.resize(len, ScreenPoint { x: 0., y: 0. });
        }
        // Only two worker tasks per emitter, rather than one pool join per short line.
        let chunk = len.div_ceil(2).max(1);
        let slots = &mut outputs[..len];
        pool.install(|| {
            slots
                .par_chunks_mut(chunk)
                .enumerate()
                .for_each(|(n, out)| {
                    input.project_chunk(n * chunk, out, mask, scaler);
                })
        });
        if let Some(work) = &self.work {
            let mut s = work.get();
            s.batches = s.batches.saturating_add(1);
            s.selected_points = s.selected_points.saturating_add(selected as u64);
            s.arena_points = s.arena_points.saturating_add(len as u64);
            s.scratch_retained_bytes =
                outputs.capacity() * std::mem::size_of::<ScreenPoint>() + CONTROL;
            work.set(s);
        }
        Some(Batch {
            input,
            scaler,
            mask,
            outputs,
            work: self
                .work
                .as_ref()
                .filter(|w| w.get().census_enabled)
                .cloned(),
            consumed: Cell::new(0),
        })
    }
}
impl Batch<'_> {
    pub(crate) fn project(
        &self,
        entry: Entry<'_>,
        index: usize,
        point: WorldPoint,
        scaler: &Scaler,
    ) -> Option<ScreenPoint> {
        if !std::ptr::eq(self.scaler, scaler) {
            return None;
        }
        let slot = entry.batch_slot(self.input, self.mask, index, point)?;
        let result = self.outputs.get(slot).copied();
        if result.is_some() && self.work.is_some() {
            self.consumed.set(self.consumed.get().saturating_add(1));
        }
        result
    }
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        if let Some(work) = &self.work {
            let mut s = work.get();
            s.consumed_points = s.consumed_points.saturating_add(self.consumed.get());
            work.set(s);
        }
    }
}

#[cfg(test)]
mod independent_oracle {
    use super::*;
    use ferrite_render::{
        DrawingInstruction, FlatProjection, LineInstruction, PortrayalOrigin, RenderContext,
        Viewport,
    };
    #[test]
    fn many_short_lines_are_ordered_and_missing_masks_decline() {
        let mut context = RenderContext::new(Viewport::new(1200., 800.));
        context
            .scaler
            .set_projection(FlatProjection::EllipsoidalMercator);
        context
            .scaler
            .set_bounds(ferrite_render::GeoBounds::new(-2., 48., 2., 52.));
        for n in 0..5000 {
            let mut line = LineInstruction::new(
                (0..4)
                    .map(|i| WorldPoint {
                        x: -1. + (n % 30) as f64 / 100.,
                        y: 49. + i as f64 / 100.,
                    })
                    .collect(),
            );
            line.portrayal_origin = PortrayalOrigin::NonPoint;
            context.add_instruction(DrawingInstruction::Line(line));
        }
        let mut arena = crate::source_line_projection_arena::Cache::new(Some("1".as_ref()));
        let scaler = &context.scaler;
        let frame = arena.prepare(&context, scaler).unwrap();
        let cache = Cache::new(Some("1".as_ref()));
        if cache.pool.is_none() {
            return;
        } // Must separately confirm actual worker availability.
        let mask = vec![true; 5000];
        let batch = cache
            .prepare(frame.projection_input(), scaler, &mask, true)
            .unwrap();
        for (ordinal, instruction) in context.raw_instructions().iter().enumerate() {
            let DrawingInstruction::Line(line) = instruction else {
                panic!("line")
            };
            let entry = frame.entry(ordinal, &line.points).unwrap();
            for (i, p) in line.points.iter().enumerate() {
                let expected = scaler.world_to_screen(*p);
                let got = batch.project(entry, i, *p, scaler).unwrap();
                assert_eq!(
                    [got.x.to_bits(), got.y.to_bits()],
                    [expected.x.to_bits(), expected.y.to_bits()]
                );
            }
            assert!(batch
                .project(entry, 0, WorldPoint { x: 0., y: 0. }, scaler)
                .is_none());
        }
        assert!(cache
            .prepare(frame.projection_input(), scaler, &mask, true)
            .is_none()); // Busy.
        drop(batch);
        // The next invocation must not consume slots left by the previous mask.
        let partial: Vec<bool> = (0..5000).map(|n| n % 2 == 0).collect();
        let mut moved = scaler.clone();
        moved.set_bounds(ferrite_render::GeoBounds::new(-2., 48., 2., 52.));
        moved.pan(123., -57.);
        moved.zoom(2.5, ScreenPoint { x: 333., y: 222. });
        let batch = cache
            .prepare(frame.projection_input(), &moved, &partial, true)
            .unwrap();
        for (ordinal, instruction) in context.raw_instructions().iter().enumerate() {
            let DrawingInstruction::Line(line) = instruction else {
                panic!("line")
            };
            let entry = frame.entry(ordinal, &line.points).unwrap();
            for (i, p) in line.points.iter().enumerate() {
                let got = batch.project(entry, i, *p, &moved);
                if partial[ordinal] {
                    let expected = moved.world_to_screen(*p);
                    let got = got.unwrap();
                    assert_eq!(
                        [got.x.to_bits(), got.y.to_bits()],
                        [expected.x.to_bits(), expected.y.to_bits()]
                    );
                } else {
                    assert!(got.is_none());
                }
                assert!(batch.project(entry, i, *p, scaler).is_none());
            }
        }
        drop(batch);
        let disabled = Cache::new(None);
        assert!(disabled
            .prepare(frame.projection_input(), scaler, &mask, true)
            .is_none());
        assert!(cache
            .prepare(frame.projection_input(), scaler, &[], true)
            .is_none());
        assert!(cache
            .prepare(frame.projection_input(), scaler, &vec![false; 5000], true)
            .is_none());
        assert!(cache
            .prepare(frame.projection_input(), scaler, &mask, false)
            .is_none());
    }
}
