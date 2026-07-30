//! Routing machinery for `agent-router`: usage readers, the decision engine, the decision log,
//! and provider dispatch.

pub mod classify;
pub mod config;
pub mod decide;
pub mod dispatch;
pub mod error;
pub mod log;
pub mod provider;
pub mod run;
pub mod runtime;
pub mod usage;

pub use classify::{Classification, Confidence, Verdict};
pub use config::Config;
pub use decide::{Decision, Gate};
pub use error::{Error, Result};
pub use provider::Provider;
pub use usage::{Headroom, UsageSnapshot};
