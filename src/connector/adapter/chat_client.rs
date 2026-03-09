// The ChatClient port trait lives in the application layer.
// Re-exported here so that connector-layer code can continue to use
// `crate::connector::adapter::ChatClient` without changes.
pub use crate::application::ChatClient;
