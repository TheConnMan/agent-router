//! MCP declaration linter for Claude and Codex project and global config.

mod parity;

pub use parity::{Difference, GlobalReport, ParityReport, ServerProjection, Status, check};
