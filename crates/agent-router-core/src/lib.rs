//! Routing machinery for `agent-router`: usage readers, the decision engine, the decision log,
//! and provider dispatch.

pub mod adversarial_review;
pub mod binary;
pub mod classify;
pub mod config;
pub mod context;
pub mod decide;
pub mod dispatch;
pub mod doctor;
pub mod error;
pub mod estimate;
pub mod log;
pub mod parity;
pub mod provider;
pub mod run;
pub mod runtime;
pub mod stats;
pub mod status;
pub mod usage;

pub use classify::Classification;
pub use config::{
    Classifier, ClassifierEngine, Config, ParityConfig, ParityException, ParityKind, Policy,
};
pub use context::Context;
pub use decide::{Decision, Gate};
pub use error::{Error, Result};
pub use parity::{Difference, GlobalReport, ParityReport, ServerProjection, Status};
pub use provider::Provider;
pub use usage::{Headroom, UsageSnapshot};
