//! Routing machinery for `agent-router`: usage readers, the decision engine, the decision log,
//! and dispatch. Every backend interaction goes through `agent-viewer-core`.

pub mod classify;
pub mod config;
pub mod decide;
pub mod error;
pub mod log;
pub mod run;
pub mod usage;

pub use classify::{Classification, Confidence, Verdict};
pub use config::Config;
pub use decide::{Decision, Gate};
pub use error::{Error, Result};
pub use usage::{Headroom, UsageSnapshot};
