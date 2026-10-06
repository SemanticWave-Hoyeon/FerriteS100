// Adapted from geoconvert 1.0.2, Copyright (c) 2024 Nicholas Crothers.
// MIT license; see LICENSE and UPSTREAM.md in this directory.
mod constants;
mod utility;
pub(super) mod transverse_mercator;
pub(super) mod polar_stereographic;
pub(super) struct LatLon { pub(super) latitude:f64, pub(super) longitude:f64 }
trait ThisOrThat {
    fn ternary<T>(&self, r#true: T, r#false: T) -> T;
    fn ternary_lazy<F, E, T>(&self, r#true: F, r#false: E) -> T
    where
        F: Fn() -> T, 
        E: Fn() -> T;
}

impl ThisOrThat for bool {
    fn ternary<T>(&self, r#true: T, r#false: T) -> T {
        if *self { r#true } else { r#false }
    }

    fn ternary_lazy<F, E, T>(&self, r#true: F, r#false: E) -> T
    where
        F: Fn() -> T, 
        E: Fn() -> T, 
    {
        if *self { r#true() } else { r#false() }
    }
}
