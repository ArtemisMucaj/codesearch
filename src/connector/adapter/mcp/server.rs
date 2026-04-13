use std::collections::HashSet;
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
use serde::{Deserialize, Serialize};

use crate::application::CallGraphQuery;
use crate::connector::api::Container;
use crate::domain::SearchQuery;

use super::tools::SearchResultOutput;

/// Server-side maximum for the number of results a single search can return.
const MAX_LIMIT: usize = 100;

/// Server-side maximum for the number of nodes a single query_graph call can return.
const MAX_QUERY_LIMIT: usize = 500;

fn default_limit() -> usize {
    10
}

fn default_query_limit() -> usize {
    50
}

fn default_text_search() -> bool {
    true
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

    /// Enable keyword (BM25) search fused with semantic search via Reciprocal Rank Fusion.
    /// Defaults to true; set to false to use only semantic (vector) search.
    #[serde(default = "default_text_search")]
    pub text_search: bool,
}

/// Input parameters for the analyze_impact tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactToolInput {
    /// Symbol name to analyse (e.g. "authenticate" or "MyStruct::new").
    /// When `regex` is true, treated as a POSIX regular expression matched
    /// against all indexed fully-qualified symbol names.
    pub symbol: String,

    /// Restrict analysis to a specific repository ID
    pub repository_id: Option<String>,

    /// When true, `symbol` is treated as a POSIX regular expression.
    /// All matching symbols are used as BFS roots and their results merged.
    /// Defaults to false.
    #[serde(default)]
    pub regex: bool,
}

/// Input parameters for the get_symbol_context tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContextToolInput {
    /// Symbol name to look up (e.g. "authenticate" or "MyStruct::new").
    /// When `regex` is true, treated as a POSIX regular expression matched
    /// against all indexed fully-qualified symbol names.
    pub symbol: String,

    /// Restrict context to a specific repository ID
    pub repository_id: Option<String>,

    /// When true, `symbol` is treated as a POSIX regular expression.
    /// All matching symbols are resolved and their edges aggregated.
    /// Defaults to false.
    #[serde(default)]
    pub regex: bool,
}

/// Input parameters for the query_graph tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryGraphInput {
    /// Relationship pattern to query. One of:
    /// callers_of, callees_of, imports_of, importers_of,
    /// inheritors_of, children_of, tests_for, file_summary
    pub pattern: String,

    /// Symbol name or file path (for file_summary) to query.
    /// Resolved with the same substring-match fallback as analyze_impact.
    pub target: String,

    /// Restrict results to a specific repository ID.
    pub repository_id: Option<String>,

    /// Maximum number of unique nodes to return (default: 50, server cap: 500).
    #[serde(default = "default_query_limit")]
    pub limit: usize,
}

/// A single deduplicated graph node returned by query_graph
#[derive(Debug, Serialize)]
pub struct GraphQueryNode {
    /// The symbol name (caller or callee depending on pattern)
    pub symbol: String,
    /// File path where the reference occurs
    pub file_path: String,
    /// Line number where the reference occurs
    pub line: u32,
    /// The kind of relationship (e.g. "call", "import", "inheritance")
    pub reference_kind: String,
    /// Repository the node belongs to
    pub repository_id: String,
}

/// Result returned by the query_graph tool
#[derive(Debug, Serialize)]
pub struct GraphQueryResult {
    /// The pattern that was queried
    pub pattern: String,
    /// The target symbol or file that was queried
    pub target: String,
    /// Deduplicated nodes matching the query
    pub nodes: Vec<GraphQueryNode>,
    /// Total number of nodes returned (after deduplication)
    pub total: usize,
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
    /// Keyword matching (BM25) fused via Reciprocal Rank Fusion is on by default; set
    /// text_search=false to use only semantic (vector) search.
    #[tool(name = "search_code")]
    async fn search_code(
        &self,
        params: Parameters<SearchToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let limit = input.limit.min(MAX_LIMIT);

        let mut query = SearchQuery::new(&input.query)
            .with_limit(limit)
            .with_text_search(input.text_search);

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
        let results = use_case
            .execute(query)
            .await
            .map_err(|e| McpError::internal_error(format!("Search failed: {}", e), None))?;

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

        let use_case = self.container.impact_use_case();
        let analysis = use_case
            .analyze(&input.symbol, input.repository_id.as_deref(), input.regex)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Impact analysis failed: {}", e), None)
            })?;

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
            .get_context(&input.symbol, input.repository_id.as_deref(), input.regex)
            .await
            .map_err(|e| McpError::internal_error(format!("Context lookup failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&ctx).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize context: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Query the call graph using an intention-named relationship pattern.
    /// Returns deduplicated graph nodes for exactly the relationship type requested,
    /// avoiding the noise of receiving all relationship kinds at once.
    ///
    /// Supported patterns:
    /// • callers_of    — who calls this symbol
    /// • callees_of    — what this symbol calls
    /// • imports_of    — what this symbol imports (Import edges only)
    /// • importers_of  — who imports this symbol (Import edges only)
    /// • inheritors_of — who inherits from / implements this symbol
    /// • children_of   — what this symbol inherits from / implements
    /// • tests_for     — test functions or files that exercise this symbol
    /// • file_summary  — all symbols referenced within a file
    ///
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "query_graph")]
    async fn query_graph(
        &self,
        params: Parameters<QueryGraphInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;
        let limit = input.limit.min(MAX_QUERY_LIMIT);

        let use_case = self.container.call_graph_use_case();

        let mut base_query = CallGraphQuery::new();
        if let Some(repo_id) = &input.repository_id {
            base_query = base_query.with_repository(repo_id.clone());
        }
        base_query = base_query.with_limit(limit as u32);

        // Each arm returns (references, use_caller).
        // use_caller=true  → node.symbol = caller_symbol (who performs the action)
        // use_caller=false → node.symbol = callee_symbol (what is acted upon)
        let (references, use_caller) = match input.pattern.as_str() {
            "callers_of" => {
                let refs = use_case
                    .find_callers(&input.target, &base_query)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, true)
            }
            "callees_of" => {
                let refs = use_case
                    .find_callees(&input.target, &base_query)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, false)
            }
            "imports_of" => {
                let q = base_query.with_reference_kind("import");
                let refs = use_case
                    .find_callees(&input.target, &q)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, false)
            }
            "importers_of" => {
                let q = base_query.with_reference_kind("import");
                let refs = use_case
                    .find_callers(&input.target, &q)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, true)
            }
            "inheritors_of" => {
                let q_inh = base_query.clone().with_reference_kind("inheritance");
                let q_imp = base_query.clone().with_reference_kind("implementation");
                let mut refs = use_case
                    .find_callers(&input.target, &q_inh)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                let mut refs2 = use_case
                    .find_callers(&input.target, &q_imp)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                refs.append(&mut refs2);
                (refs, true)
            }
            "children_of" => {
                let q_inh = base_query.clone().with_reference_kind("inheritance");
                let q_imp = base_query.clone().with_reference_kind("implementation");
                let mut refs = use_case
                    .find_callees(&input.target, &q_inh)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                let mut refs2 = use_case
                    .find_callees(&input.target, &q_imp)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                refs.append(&mut refs2);
                (refs, false)
            }
            "tests_for" => {
                let refs = use_case
                    .find_callers(&input.target, &base_query)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                let filtered: Vec<_> = refs
                    .into_iter()
                    .filter(|r| {
                        let sym = r.caller_symbol().unwrap_or("").to_lowercase();
                        let file = r.reference_file_path().to_lowercase();
                        sym.starts_with("test_")
                            || sym.ends_with("_test")
                            || sym.ends_with("_spec")
                            || file.contains("test")
                            || file.contains("spec")
                    })
                    .collect();
                (filtered, true)
            }
            "file_summary" => {
                let refs = use_case
                    .find_by_file(&input.target, &base_query)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, false)
            }
            unknown => {
                return Err(McpError::internal_error(
                    format!(
                        "Unknown pattern '{}'. Supported patterns: callers_of, callees_of, \
                         imports_of, importers_of, inheritors_of, children_of, tests_for, \
                         file_summary",
                        unknown
                    ),
                    None,
                ));
            }
        };

        // Deduplicate by symbol name, keeping the first reference site per unique symbol.
        let mut seen: HashSet<String> = HashSet::new();
        let nodes: Vec<GraphQueryNode> = references
            .into_iter()
            .filter_map(|r| {
                let symbol = if use_caller {
                    r.caller_symbol()
                        .unwrap_or_else(|| r.caller_file_path())
                        .to_string()
                } else {
                    r.callee_symbol().to_string()
                };
                if symbol.is_empty() || !seen.insert(symbol.clone()) {
                    return None;
                }
                Some(GraphQueryNode {
                    symbol,
                    file_path: r.reference_file_path().to_string(),
                    line: r.reference_line(),
                    reference_kind: r.reference_kind().as_str().to_string(),
                    repository_id: r.repository_id().to_string(),
                })
            })
            .take(limit)
            .collect();

        let total = nodes.len();
        let result = GraphQueryResult {
            pattern: input.pattern,
            target: input.target,
            nodes,
            total,
        };

        let json = serde_json::to_string_pretty(&result).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize result: {}", e), None)
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
                 • search_code — find code by natural language description (set text_search=false \
                   to disable keyword+semantic fusion)\n\
                 • analyze_impact — blast-radius analysis: what breaks if symbol X changes?\n\
                 • get_symbol_context — 360° view of a symbol's callers and callees\n\
                 • query_graph — precise relationship queries: callers_of, callees_of, \
                   imports_of, importers_of, inheritors_of, children_of, tests_for, file_summary"
                    .into(),
            ),
        }
    }
}
