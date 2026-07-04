//! Python detectors, one file per library.

mod flask_fastapi;
mod kafka_python;
mod paho_mqtt;
mod requests;

use super::Detector;

/// Every Python library detector.
pub(super) fn detectors() -> Vec<Detector> {
    let mut all = Vec::new();
    all.extend(kafka_python::detectors());
    all.extend(paho_mqtt::detectors());
    all.extend(flask_fastapi::detectors());
    all.extend(requests::detectors());
    all
}
