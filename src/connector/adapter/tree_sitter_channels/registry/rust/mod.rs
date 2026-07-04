//! Rust detectors, one file per library.

mod axum;
mod reqwest;

use super::Detector;

/// Every Rust library detector.
pub(super) fn detectors() -> Vec<Detector> {
    let mut all = Vec::new();
    all.extend(axum::detectors());
    all.extend(reqwest::detectors());
    all
}
