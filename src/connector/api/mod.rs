pub mod container;
pub mod controller;
pub mod copilot_command;
pub mod openai_command;
pub mod repo_resolver;
pub mod router;

pub use container::{Container, ContainerConfig};
pub use copilot_command::run as run_copilot_command;
pub use openai_command::run as run_openai_command;
pub use repo_resolver::{
    namespace_embedding_config, repositories_by_namespace, resolve as resolve_repo_context,
    ResolvedContext,
};
pub use router::Router;
