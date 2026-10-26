//! S-100 chart engine: the domain logic of FerriteS100 that needs no window,
//! GUI or GPU (see `docs/ENGINE_DESIGN.md`).
//!
//! Stage 1 holds the pure domain modules moved out of the application:
//! dataset discovery and open planning, signature policy, the S-101 update
//! chain and lifecycle metadata, exchange-set checks, cell publication,
//! catalogue selection, decoded/instruction caches, the S-102 input and
//! journal policies, S-421 route input and the non-UI S-124/S-129 overlay
//! parts. Behaviour is unchanged from the application modules.

pub mod cell_source_identity;
pub mod chart_publication;
pub mod dataset_discovery;
pub mod dataset_open_plan;
pub mod dataset_pick_fields;
pub mod dataset_signature_policy;
pub mod decoded_chart_cache;
pub mod gml_overlay;
pub mod interoperability;
pub mod s101_catalogue_selection;
pub mod s101_lifecycle_metadata;
pub mod s101_update_plan;
pub mod s101_xc_capture_pool;
pub mod s101_xc_consistency;
pub mod s101_xc_coverage;
pub mod s101_xc_region;
pub mod s102_depth_policy;
pub mod s102_input_capture;
pub mod s102_journal_admission;
pub mod s102_original_inputs;
pub mod s102_trial_worker;
pub mod s421_dataset_input;
pub mod secure_instruction_cache;

mod chart_load;
pub use chart_load::{CellHydrationSource, ChartLoadResult};
