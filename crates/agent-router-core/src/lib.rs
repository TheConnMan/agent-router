//! Routing machinery for `agent-router`: usage readers, the decision engine, the decision log,
//! and dispatch. Every backend interaction goes through `agent-viewer-core`.

pub mod config;
pub mod error;
pub mod usage;

pub use config::Config;
pub use error::{Error, Result};
pub use usage::{Headroom, UsageSnapshot};
