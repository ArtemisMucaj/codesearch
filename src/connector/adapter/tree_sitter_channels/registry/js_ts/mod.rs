//! JavaScript / TypeScript detectors, one file per library.
//!
//! JS and TS share tree-sitter grammar shapes, so every detector here is
//! emitted once per language. Library files build their detectors through
//! [`for_both_languages`] rather than hard-coding a language, which keeps the
//! JS/TS duplication in one place.

mod axios;
mod express;
mod fetch;
mod kafka_positional;
mod kafkajs;
mod mqtt_js;

use super::Detector;
use crate::domain::Language;

/// The two languages that share these grammar shapes.
const JS_TS: [Language; 2] = [Language::JavaScript, Language::TypeScript];

/// Emit one detector per JS/TS language from a template keyed on the language.
///
/// Library files describe a detector as a closure `Language -> Detector`; this
/// expands it across [`JS_TS`] so a library never repeats the language list.
pub(super) fn for_both_languages(build: impl Fn(Language) -> Detector) -> Vec<Detector> {
    JS_TS.into_iter().map(build).collect()
}

/// Every JS/TS library detector.
pub(super) fn detectors() -> Vec<Detector> {
    let mut all = Vec::new();
    all.extend(express::detectors());
    all.extend(axios::detectors());
    all.extend(fetch::detectors());
    all.extend(kafkajs::detectors());
    all.extend(kafka_positional::detectors());
    all.extend(mqtt_js::detectors());
    all
}
