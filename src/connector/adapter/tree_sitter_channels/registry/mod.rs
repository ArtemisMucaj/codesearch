//! Detector registry: every framework integration is data, not code.
//!
//! A detector is a tree-sitter query with a `@channel` capture (the argument
//! holding the channel name — a string literal, or an identifier recorded as
//! an unresolved endpoint) plus capture filters that pin the query to a
//! framework's call shape. Adding a framework is one entry here and one
//! fixture test.
//!
//! The registry is organised **per language, then per library**: each
//! language has a submodule (`python`, `js_ts`, `rust`), and within it each
//! third-party library gets its own file exporting a `detectors()` function.
//! Integrating a new library is a new file plus one line in the language's
//! `mod.rs` — no existing detector is touched.

mod js_ts;
mod python;
mod rust;

use crate::domain::{ChannelRole, Language, Protocol};

/// A single framework detector.
pub(crate) struct Detector {
    pub language: Language,
    pub protocol: Protocol,
    pub role: ChannelRole,
    /// Tree-sitter S-expression containing a `@channel` capture.
    pub query: &'static str,
    /// Each named capture must equal one of the allowed values for the match
    /// to count (evaluated in Rust; keeps the queries predicate-free).
    pub filters: &'static [(&'static str, &'static [&'static str])],
    /// Extraction confidence for endpoints produced by this detector.
    pub confidence: f32,
}

/// HTTP verbs used by client libraries (request methods).
pub(crate) const HTTP_CLIENT_METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "head"];
/// HTTP verbs used by server frameworks to register routes.
pub(crate) const HTTP_SERVER_METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "all"];

/// Every detector across every language and library.
///
/// Precision note: method-name matching (e.g. any `.send(...)`) occasionally
/// fires on unrelated objects. That is acceptable here — a false endpoint only
/// becomes a false edge if an opposite-role endpoint exists on the same
/// channel string, so the join itself filters most noise; confidence scoring
/// covers the rest. Detectors bound to an unambiguous shape (constructor
/// name, decorator, `reqwest::` path) get higher confidence than bare method
/// names.
pub(crate) fn detectors() -> Vec<Detector> {
    let mut all = Vec::new();
    all.extend(python::detectors());
    all.extend(js_ts::detectors());
    all.extend(rust::detectors());
    all
}
