use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::connector::api::Container;
use crate::domain::SearchQuery;

use super::tools::SearchResultOutput;

/// Server-side maximum for the number of results a single search can return.
const MAX_LIMIT: usize = 100;

fn default_limit() -> usize {
    10
}

/// Input parameters for the search_code tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchToolInput {
    /// Natural language query describing the code you're looking for
    pub query: String,

    /// Maximum number of results to return (default: 10, server cap: 100)
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Minimum relevance score threshold (0.0 to 1.0)
    pub min_score: Option<f32>,

    /// Filter results by programming languages (e.g., ["rust", "python"])
    pub languages: Option<Vec<String>>,

    /// Filter results by repository IDs
    pub repositories: Option<Vec<String>>,
}

/// MCP Server that exposes codesearch functionality
#[derive(Clone)]
pub struct CodesearchMcpServer {
    container: Arc<Container>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl CodesearchMcpServer {
    pub fn new(container: Arc<Container>) -> Self {
        Self {
            container,
            tool_router: Self::tool_router(),
        }
    }

    /// Search for code using semantic similarity. Returns relevant code snippets matching a natural language query.
    /// Use this to find functions, classes, implementations, or any code constructs by describing what you're looking for.
    #[tool(name = "search_code")]
    async fn search_code(
        &self,
        params: Parameters<SearchToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        // Clamp to server-side maximum
        let limit = input.limit.min(MAX_LIMIT);

        // Build SearchQuery from input
        let mut query = SearchQuery::new(&input.query).with_limit(limit);

        if let Some(score) = input.min_score {
            query = query.with_min_score(score);
        }
        if let Some(langs) = input.languages {
            query = query.with_languages(langs);
        }
        if let Some(repos) = input.repositories {
            query = query.with_repositories(repos);
        }

        // Execute search
        let use_case = self.container.search_use_case();
        let results = use_case.execute(query).await.map_err(|e| {
            McpError::internal_error(format!("Search failed: {}", e), None)
        })?;

        // Convert to output format
        let outputs: Vec<SearchResultOutput> = results
            .iter()
            .map(|r| SearchResultOutput {
                file_path: r.chunk().file_path().to_string(),
                start_line: r.chunk().start_line(),
                end_line: r.chunk().end_line(),
                score: r.score(),
                language: r.chunk().language().to_string(),
                node_type: r.chunk().node_type().to_string(),
                symbol_name: r.chunk().symbol_name().map(String::from),
                content: r.chunk().content().to_string(),
                repository_id: r.chunk().repository_id().to_string(),
            })
            .collect();

        // Return as JSON content
        let json = serde_json::to_string_pretty(&outputs).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize results: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for CodesearchMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Semantic code search server. Use the search_code tool to find relevant code \
                 snippets by describing what you're looking for in natural language. The tool \
                 searches across indexed repositories using ML embeddings for semantic similarity."
                    .into(),
            ),
        }
    }
}
