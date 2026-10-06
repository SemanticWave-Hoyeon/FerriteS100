//! Conservative selection of positive-down depths in one explicitly named reference.
//!
//! A reference key identifies a datum surface within a host-defined scope. A product's
//! datum classification code alone does not establish that two surfaces are identical.
//! Transform discovery, spatial validity, units and product encoding stay in adapters.
use anyhow::{ensure, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthReference(pub u64);

/// Stable source identity and original node, never the resampled output node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DepthSource {
    pub instance: usize,
    pub column: usize,
    pub row: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct DepthCandidate {
    pub source: DepthSource,
    pub reference: DepthReference,
    pub x: f64,
    pub y: f64,
    pub raw_depth: Option<f64>,
    /// Source uncertainty is retained; no transformation of its statistics is implied.
    pub uncertainty: Option<f64>,
}

/// Positive-down convention: adjusted depth = raw depth + correction_metres.
/// Provenance is an opaque host identifier for the applied transformation evidence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthAdjustment {
    pub correction_metres: f64,
    pub provenance: u64,
}

pub trait DepthAdjustmentProvider: Send + Sync {
    /// `None` means unavailable, not an identity transformation. Evaluate at the
    /// comparison position, which may differ from a source grid's nearest node.
    fn adjustment(
        &self,
        from: DepthReference,
        to: DepthReference,
        x: f64,
        y: f64,
    ) -> Result<Option<DepthAdjustment>>;
}

/// Only scoped identity is known. No datum-code-based or cross-file inference.
pub struct IdentityDepthAdjustment;
impl DepthAdjustmentProvider for IdentityDepthAdjustment {
    fn adjustment(
        &self,
        from: DepthReference,
        to: DepthReference,
        _: f64,
        _: f64,
    ) -> Result<Option<DepthAdjustment>> {
        Ok((from == to).then_some(DepthAdjustment {
            correction_metres: 0.0,
            provenance: 0,
        }))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SelectedDepth {
    pub candidate: DepthCandidate,
    pub target_reference: DepthReference,
    pub adjustment: DepthAdjustment,
    /// Remains f64 through portrayal classification; casting to f32 can cross a contour.
    pub adjusted_depth: f64,
}

/// O(n) time and O(1) space per comparison. Equal adjusted values use stable source
/// identity, independent of iteration/drawing order. Every populated candidate must
/// have an available transformation: an unknown competitor cannot safely be skipped.
/// A failed accumulator is poisoned so ignoring `consider` errors cannot yield a winner.
pub struct ShoalestDepth {
    target: DepthReference,
    x: f64,
    y: f64,
    selected: Option<SelectedDepth>,
    failed: bool,
}
impl ShoalestDepth {
    pub fn new(target: DepthReference, x: f64, y: f64) -> Result<Self> {
        ensure!(
            x.is_finite() && y.is_finite(),
            "Non-finite depth comparison position"
        );
        Ok(Self {
            target,
            x,
            y,
            selected: None,
            failed: false,
        })
    }

    pub fn consider(
        &mut self,
        candidate: DepthCandidate,
        provider: &impl DepthAdjustmentProvider,
    ) -> Result<()> {
        ensure!(!self.failed, "Depth selection previously failed");
        let result = self.consider_validated(candidate, provider);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn consider_validated(
        &mut self,
        candidate: DepthCandidate,
        provider: &impl DepthAdjustmentProvider,
    ) -> Result<()> {
        let Some(raw) = candidate.raw_depth else {
            return Ok(());
        };
        ensure!(raw.is_finite(), "Non-finite source depth");
        ensure!(
            candidate.x.is_finite() && candidate.y.is_finite(),
            "Non-finite source node"
        );
        ensure!(
            candidate
                .uncertainty
                .is_none_or(|v| v.is_finite() && v >= 0.0),
            "Invalid source uncertainty"
        );
        let adjustment = provider
            .adjustment(candidate.reference, self.target, self.x, self.y)?
            .with_context(|| {
                format!(
                    "Missing depth adjustment for source {:?} from {:?} to {:?}",
                    candidate.source, candidate.reference, self.target
                )
            })?;
        ensure!(
            adjustment.correction_metres.is_finite(),
            "Non-finite depth correction"
        );
        let adjusted_depth = raw + adjustment.correction_metres;
        ensure!(adjusted_depth.is_finite(), "Adjusted depth overflow");
        let selected = SelectedDepth {
            candidate,
            target_reference: self.target,
            adjustment,
            adjusted_depth,
        };
        if self.selected.is_none_or(|old| {
            adjusted_depth < old.adjusted_depth
                || (adjusted_depth == old.adjusted_depth && candidate.source < old.candidate.source)
        }) {
            self.selected = Some(selected);
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Option<SelectedDepth>> {
        ensure!(!self.failed, "Cannot finalize failed depth selection");
        Ok(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(instance: usize, reference: u64, raw: Option<f64>) -> DepthCandidate {
        DepthCandidate {
            source: DepthSource {
                instance,
                column: 17,
                row: 23,
            },
            reference: DepthReference(reference),
            x: 10.,
            y: 20.,
            raw_depth: raw,
            uncertainty: Some(0.25),
        }
    }
    struct Adjust;
    impl DepthAdjustmentProvider for Adjust {
        fn adjustment(
            &self,
            from: DepthReference,
            to: DepthReference,
            x: f64,
            y: f64,
        ) -> Result<Option<DepthAdjustment>> {
            assert_eq!((to, x, y), (DepthReference(1), 11., 21.));
            Ok(match from.0 {
                1 => Some(DepthAdjustment {
                    correction_metres: 0.,
                    provenance: 0,
                }),
                2 => Some(DepthAdjustment {
                    correction_metres: -4.,
                    provenance: 42,
                }),
                _ => None,
            })
        }
    }
    #[test]
    fn adjusted_not_raw_minimum_and_source_provenance() {
        for reverse in [false, true] {
            let mut candidates = [sample(0, 1, Some(10.)), sample(1, 2, Some(12.))];
            if reverse {
                candidates.reverse();
            }
            let mut selection = ShoalestDepth::new(DepthReference(1), 11., 21.).unwrap();
            for c in candidates {
                selection.consider(c, &Adjust).unwrap();
            }
            let winner = selection.finish().unwrap().unwrap();
            assert_eq!(winner.adjusted_depth, 8.);
            assert_eq!(winner.candidate.raw_depth, Some(12.));
            assert_eq!(
                winner.candidate.source,
                DepthSource {
                    instance: 1,
                    column: 17,
                    row: 23
                }
            );
            assert_eq!(winner.candidate.uncertainty, Some(0.25));
            assert_eq!(winner.adjustment.provenance, 42);
        }
    }
    #[test]
    fn unavailable_competitor_poisoned_and_nodata_is_not_zero() {
        let mut s = ShoalestDepth::new(DepthReference(1), 11., 21.).unwrap();
        s.consider(sample(0, 1, Some(10.)), &Adjust).unwrap();
        s.consider(sample(2, 999, None), &Adjust).unwrap();
        assert!(s.consider(sample(1, 999, Some(100.)), &Adjust).is_err());
        assert!(s.finish().is_err());
        let mut s = ShoalestDepth::new(DepthReference(1), 11., 21.).unwrap();
        s.consider(sample(0, 999, None), &Adjust).unwrap();
        assert!(s.finish().unwrap().is_none());
    }
    #[test]
    fn stable_tie_independent_of_iteration_and_signed_zero() {
        for order in [[2, 0, 1], [1, 0, 2]] {
            let mut s = ShoalestDepth::new(DepthReference(1), 0., 0.).unwrap();
            for i in order {
                s.consider(
                    sample(i, 1, Some(if i == 0 { -0.0 } else { 0.0 })),
                    &IdentityDepthAdjustment,
                )
                .unwrap();
            }
            assert_eq!(s.finish().unwrap().unwrap().candidate.source.instance, 0);
        }
    }
    #[test]
    fn no_implicit_transformation_and_invalid_numbers_rejected() {
        for c in [
            sample(0, 2, Some(1.)),
            sample(0, 1, Some(f64::NAN)),
            sample(0, 1, Some(f64::INFINITY)),
        ] {
            let mut s = ShoalestDepth::new(DepthReference(1), 0., 0.).unwrap();
            assert!(s.consider(c, &IdentityDepthAdjustment).is_err());
            assert!(s.finish().is_err());
        }
        assert!(ShoalestDepth::new(DepthReference(1), f64::NAN, 0.).is_err());
        struct Bad(f64);
        impl DepthAdjustmentProvider for Bad {
            fn adjustment(
                &self,
                _: DepthReference,
                _: DepthReference,
                _: f64,
                _: f64,
            ) -> Result<Option<DepthAdjustment>> {
                Ok(Some(DepthAdjustment {
                    correction_metres: self.0,
                    provenance: 1,
                }))
            }
        }
        for correction in [f64::NAN, f64::INFINITY, f64::MAX] {
            let mut s = ShoalestDepth::new(DepthReference(1), 0., 0.).unwrap();
            assert!(s
                .consider(sample(0, 1, Some(f64::MAX)), &Bad(correction))
                .is_err());
            assert!(s.finish().is_err());
        }
    }
    #[test]
    fn keeps_adjusted_precision_at_portrayal_threshold() {
        struct Fine;
        impl DepthAdjustmentProvider for Fine {
            fn adjustment(
                &self,
                _: DepthReference,
                _: DepthReference,
                _: f64,
                _: f64,
            ) -> Result<Option<DepthAdjustment>> {
                Ok(Some(DepthAdjustment {
                    correction_metres: -1e-8,
                    provenance: 9,
                }))
            }
        }
        let mut s = ShoalestDepth::new(DepthReference(1), 0., 0.).unwrap();
        s.consider(sample(0, 2, Some(30.)), &Fine).unwrap();
        let value = s.finish().unwrap().unwrap().adjusted_depth;
        assert!(value < 30.);
        assert_eq!(value as f32, 30.); // demonstrates why f32 would change classification
    }
}
