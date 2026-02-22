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
/// Hard cap on BFS depth for impact analysis to prevent unbounded traversal.
const MAX_DEPTH: usize = 20;

fn default_limit() -> usize {
    10
}

fn default_depth() -> usize {
    5
}

// ── Input types ──────────────────────────────────────────────────────────────

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

    /// Combine keyword (BM25) and semantic search via Reciprocal Rank Fusion.
    /// Improves results for exact symbol names and rare identifiers.
    #[serde(default)]
    pub hybrid: bool,
}

/// Input parameters for the analyze_impact tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactToolInput {
    /// Symbol name to analyse (e.g. "authenticate" or "MyStruct::new")
    pub symbol: String,

    /// Maximum hop depth to traverse (default: 5)
    #[serde(default = "default_depth")]
    pub depth: usize,

    /// Restrict analysis to a specific repository ID
    pub repository_id: Option<String>,
}

/// Input parameters for the get_symbol_context tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContextToolInput {
    /// Symbol name to look up (e.g. "authenticate" or "MyStruct::new")
    pub symbol: String,

    /// Restrict context to a specific repository ID
    pub repository_id: Option<String>,

    /// Maximum number of callers/callees to return per direction
    pub limit: Option<u32>,
}

// ── MCP Server ───────────────────────────────────────────────────────────────

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

    /// Search for code using semantic similarity. Returns relevant code snippets matching a
    /// natural language query. Use this to find functions, classes, implementations, or any
    /// code constructs by describing what you're looking for.
    /// Set hybrid=true to also run keyword matching (BM25) fused via Reciprocal Rank Fusion —
    /// this helps when searching for exact symbol names or rare identifiers.
    #[tool(name = "search_code")]
    async fn search_code(
        &self,
        params: Parameters<SearchToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let limit = input.limit.min(MAX_LIMIT);

        let mut query = SearchQuery::new(&input.query)
            .with_limit(limit)
            .with_hybrid(input.hybrid);

        if let Some(score) = input.min_score {
            query = query.with_min_score(score);
        }
        if let Some(langs) = input.languages {
            query = query.with_languages(langs);
        }
        if let Some(repos) = input.repositories {
            query = query.with_repositories(repos);
        }

        let use_case = self.container.search_use_case();
        let results = use_case.execute(query).await.map_err(|e| {
            McpError::internal_error(format!("Search failed: {}", e), None)
        })?;

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

        let json = serde_json::to_string_pretty(&outputs).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize results: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Analyse the blast radius of changing a symbol.
    /// Performs a BFS through the call graph to find every symbol that directly or
    /// transitively calls (or depends on) the given symbol, grouped by hop depth.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "analyze_impact")]
    async fn analyze_impact(
        &self,
        params: Parameters<ImpactToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;
        let depth = input.depth.min(MAX_DEPTH);

        let use_case = self.container.impact_use_case();
        let analysis = use_case
            .analyze(&input.symbol, depth, input.repository_id.as_deref())
            .await
            .map_err(|e| McpError::internal_error(format!("Impact analysis failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&analysis).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize impact analysis: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get the 360-degree context for a symbol: who calls it (callers) and what it
    /// calls (callees). Useful for understanding a symbol's role in the codebase
    /// before refactoring or debugging.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "get_symbol_context")]
    async fn get_symbol_context(
        &self,
        params: Parameters<ContextToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let use_case = self.container.context_use_case();
        let ctx = use_case
            .get_context(&input.symbol, input.repository_id.as_deref(), input.limit)
            .await
            .map_err(|e| McpError::internal_error(format!("Context lookup failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&ctx).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize context: {}", e), None)
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
                "Semantic code search server. Available tools:\n\
                 • search_code — find code by natural language description (add hybrid=true \
                   for keyword+semantic fusion)\n\
                 • analyze_impact — blast-radius analysis: what breaks if symbol X changes?\n\
                 • get_symbol_context — 360° view of a symbol's callers and callees"
                    .into(),
            ),
        }
    }
}
