//! roundtable: one terminal chat for Claude Code, Codex and other models.
//! The binary (`src/main.rs`) runs the terminal; the logic lives here.

pub mod agent;
pub mod app;
pub mod mcp;
pub mod openai;
pub mod registry;
pub mod transcript;
pub mod writer_lock;
