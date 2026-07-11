//! Management HTTP API adapter.
//!
//! A REST/JSON server that exposes codesearch operations over HTTP, separate
//! from the MCP protocol server. Both run side by side under the `serve`
//! subcommand (see `src/main.rs`).
//!
//! This PR (1/3) ships the skeleton only: bootstrap, shared state, a `/health`
//! endpoint, an API index, and graceful shutdown. Later PRs attach the actual
//! command endpoints (PR2) and streaming endpoints (PR3) by extending
//! [`server::routes`] — see that function's docs for the exact extension point.

mod server;

pub use server::{routes, run_management_server, AppState};
