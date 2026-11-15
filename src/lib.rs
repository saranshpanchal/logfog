//! LogFog — one sparse index, two faces.
//!
//! Raw logs stay on disk (or in a ring buffer for pipes). A packed block index
//! records offsets, timestamps, levels, and template ids. The browser explorer
//! and the agent CLI/MCP server read that same index.

pub mod cli;
pub mod drain;
pub mod engine;
pub mod follow;
pub mod index;
pub mod mcp;
pub mod parser;
pub mod query;
pub mod session;
pub mod sources;
pub mod timeutil;
pub mod tui;
pub mod types;
pub mod web;

pub use engine::Engine;
pub use types::{FacetValue, Level, Origin, SourceKind};
