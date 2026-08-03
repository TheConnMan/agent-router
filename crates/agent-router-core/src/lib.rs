//! Routing machinery for `agent-router`: usage readers, the decision engine, the decision log,
//! and provider dispatch.

pub mod classify;
pub mod cloud;
pub mod config;
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
pub use cloud::{CloudEligibility, CloudIneligible, CloudTarget};
pub use config::{
    Classifier, ClassifierEngine, Config, DefaultProvider, ParityConfig, ParityException,
    ParityKind, Policy,
};
pub use decide::{Decision, ExecutionTarget, Gate};
pub use error::{Error, Result};
pub use parity::{Difference, GlobalReport, ParityReport, ServerProjection, Status};
pub use provider::Provider;
pub use usage::{Headroom, UsageSnapshot};
