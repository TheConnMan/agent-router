//! Routing machinery for `agent-router`: usage readers, the decision engine, the decision log,
//! and dispatch. Every backend interaction goes through `agent-viewer-core`.

pub mod classify;
pub mod config;
pub mod decide;
pub mod dispatch;
pub mod error;
pub mod log;
pub mod parity;
pub mod run;
pub mod usage;

pub use classify::{Classification, Confidence, Verdict};
pub use config::{Config, DefaultProvider, ParityConfig, ParityException, ParityKind, Policy};
pub use decide::{Decision, Gate};
pub use error::{Error, Result};
pub use parity::{Difference, ParityReport, ServerProjection, Status};
pub use usage::{Headroom, UsageSnapshot};
