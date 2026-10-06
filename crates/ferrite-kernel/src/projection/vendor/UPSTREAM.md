Projection mathematics adapted from geoconvert 1.0.2 (MIT, Nicholas Crothers), itself a Rust translation of GeographicLib.
Upstream: https://github.com/ncrothers/geoconvert-rs
Registry archive: https://static.crates.io/crates/geoconvert/geoconvert-1.0.2.crate
Archive SHA256: fd16cfb2aaf20ea861e7d55bea8dff83dd18257b4022badb26ae6ba03d6800c2

Only four projection/math files, their MIT license, the bool helper trait and a private raw LatLon holder are retained. Import paths are localized. num::{Complex,Integer} uses existing separately pinned num-complex/num-integer rather than the num facade. No automatic zone choice, MGRS, serde or native-library dependency is imported. Projection algorithms/coefficient tables are unchanged. Numerical qualification belongs to the public checked adapter. Whole-footprint geographic enclosure and hardware rendering error remain separate obligations; these point operations do not certify either.
