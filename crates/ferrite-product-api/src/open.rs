//! Background opening of datasets by a host open job.
//!
//! A host job (the engine's `ChartEngine::open`) runs on a worker thread
//! with the stages of the registered layers ([`crate::ProductLayer::open_stage`],
//! in registration order):
//!
//! 1. every input path is offered to the stages in order; the first whose
//!    [`OpenStage::sniff`] recognises it and whose [`OpenStage::accept`]
//!    takes it owns it (the job reads the head once, only for stages that
//!    need it: [`OpenStage::sniffs_head`]);
//! 2. [`OpenStage::after_claims`] lets a stage report on what it took;
//! 3. [`OpenStage::load`] reads, verifies and validates the taken inputs
//!    into a result the layer installs later on the owner thread
//!    ([`crate::ProductLayer::install`]).
use crate::platform::{DataSource, Executor, UserStore};
use crate::report::{InstallReport, ValidationIssue};
use crate::Recognised;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether dataset signatures must verify before data is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// Open without verification (datasets are marked unverified).
    Off,
    /// Reject unsigned or invalid datasets (S-100 Part 15). Exchange sets are
    /// verified through the platform data source (files or bytes) and S-101
    /// chains are planned from the authenticated snapshots, as the viewer
    /// does for local folders.
    Required,
}

/// The job's signature verification of a set of inputs: the
/// `ferrite_security::AuthorizedDatasets` report, boxed so the interface
/// does not depend on the signature backends.
pub type Authorize<'a> = dyn Fn(&[PathBuf]) -> anyhow::Result<Box<dyn Any>> + Sync + 'a;

/// What a stage reads and verifies through.
pub struct OpenContext<'a> {
    pub data: &'a dyn DataSource,
    pub executor: &'a dyn Executor,
    pub signatures: SignaturePolicy,
    /// Inputs were selected explicitly (files, bytes) rather than found in
    /// a folder.
    pub explicit: bool,
    pub authorize: &'a Authorize<'a>,
    /// Per-user records (S-101 cancellation history).
    pub user: &'a dyn UserStore,
    pub cancel: &'a AtomicBool,
}

impl OpenContext<'_> {
    /// Signatures must verify.
    pub fn required(&self) -> bool {
        self.signatures == SignaturePolicy::Required
    }
    /// The job was cancelled.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    /// Fail with "Open cancelled" when the job was cancelled.
    pub fn check_cancelled(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancelled(), "Open cancelled");
        Ok(())
    }
}

/// Findings of an open job, in the order they arose.
#[derive(Debug, Default)]
pub struct OpenNotes {
    pub issues: Vec<ValidationIssue>,
    pub notices: Vec<String>,
}

/// One layer's part of an open job (moved to the worker).
pub trait OpenStage: Send {
    /// Whether this stage may recognise `name` only from its head (the job
    /// then reads the first 64 KiB once).
    fn sniffs_head(&self, _name: &Path) -> bool {
        false
    }
    /// Whether `name` (with `head`, empty when not read) is one of the
    /// stage's datasets: the products' adapter detection.
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised>;
    /// Take a recognised input (true); false leaves it to later stages.
    /// Stages that capture inputs while classifying report failures here.
    fn accept(&mut self, path: &Path, context: &OpenContext<'_>, notes: &mut OpenNotes) -> bool;
    /// Inputs taken that count toward the job's dataset input limit.
    fn counted_inputs(&self) -> usize {
        0
    }
    /// After every input was offered.
    fn after_claims(&mut self, _notes: &mut OpenNotes) {}
    /// Read, verify and validate the taken inputs.
    fn load(
        self: Box<Self>,
        context: &OpenContext<'_>,
        notes: &mut OpenNotes,
    ) -> anyhow::Result<Loaded>;
}

/// What a stage loaded, for its layer's [`crate::ProductLayer::install`].
pub struct Loaded {
    pub payload: Box<dyn Any + Send>,
    /// Loaded inputs by kind (`"cells"`, `"cancellations"`, `"overlays"`,
    /// `"bathymetry"`, `"routes"`), for hosts that report job sizes.
    pub counts: Vec<(&'static str, usize)>,
}

/// What installing reads through on the owner thread.
pub struct InstallContext<'a> {
    pub user: &'a dyn UserStore,
}

/// Convenience for layers: an install report entry for an input that was
/// already open.
pub fn already_open(report: &mut InstallReport, path: &Path) {
    report
        .notices
        .push(format!("Already open: {}", path.display()));
}
