//! Management HTTP API adapter.
//!
//! A REST/JSON server that exposes codesearch operations over HTTP, separate
//! from the MCP protocol server. Both run side by side under the `serve`
//! subcommand (see `src/main.rs`).
//!
//! PR1 shipped the skeleton (bootstrap, shared state, `/health`, API index,
//! graceful shutdown). PR2 attaches non-streaming command endpoints. This
//! module also carries PR3's streaming layer: [`streaming`] holds the SSE
//! handlers (explain / index) mounted under `/api/stream/...`, plus the
//! `/api/openapi.json` document, all wired in [`server::routes`].

mod copilot_login;
mod dream;
mod error;
mod handlers;
mod server;
mod session_import;
mod streaming;

pub use copilot_login::CopilotLoginService;
pub use dream::{DreamService, MemoryConfigPatch};
pub use server::{routes, run_management_server, AppState};
pub use session_import::SessionImportService;
