//! Agent integration: wiring codesearch into AI coding assistants so they reach
//! for semantic search instead of grepping.
//!
//! This module extends the ecosystem *around* codesearch rather than its core
//! search/analysis engine. It has two halves:
//!
//! * [`install`] — everything that writes (and removes) the per-platform hooks,
//!   plugins, rules, and git hooks. Driven by the `install`/`uninstall`/`hooks`
//!   CLI commands.
//! * [`pre_tool_call`] — the runtime command those hooks invoke to decide
//!   whether to nudge the assistant toward codesearch.

pub mod install;
pub mod pre_tool_call;
