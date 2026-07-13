use std::collections::HashSet;
use std::path::Path;
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

use crate::application::{CallGraphQuery, ChannelLinkOptions};
use crate::connector::api::Container;
use crate::domain::{FileEdge, GraphLevel, MemoryKind, Protocol, SearchQuery};

use super::tools::SearchResultOutput;

/// Server-side maximum for the number of results a single search can return.
const MAX_LIMIT: usize = 100;

/// Server-side maximum for the number of execution features `list_features` can
/// return. Caps caller-supplied limits so a huge value cannot trigger unbounded
/// call-graph traversal and serialization.
const MAX_FEATURES_LIMIT: usize = 100;

fn default_limit() -> usize {
    10
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

/// Relationship pattern for the query_graph tool.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryPattern {
    CallersOf,
    CalleesOf,
    ImportsOf,
    ImportersOf,
    InheritorsOf,
    ChildrenOf,
    TestsFor,
    FileSummary,
}

/// Input parameters for the query_graph tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryGraphInput {
    /// Relationship pattern to query.
    pub pattern: QueryPattern,

    /// Symbol name or file path (for file_summary) to query.
    /// Resolved with the same substring-match fallback as analyze_impact.
    pub target: String,

    /// Restrict results to a specific repository ID.
    pub repository_id: Option<String>,

    /// Maximum number of unique nodes to return. Omit to return all results.
    pub limit: Option<usize>,
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
    pub pattern: QueryPattern,
    /// The target symbol or file that was queried
    pub target: String,
    /// Deduplicated nodes matching the query
    pub nodes: Vec<GraphQueryNode>,
    /// Total number of nodes returned (after deduplication; equals len(nodes))
    pub total: usize,
}

fn default_features_limit() -> usize {
    20
}

/// Input parameters for the list_repositories tool (takes no arguments).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRepositoriesInput {}

/// Input parameters for the list_features tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFeaturesInput {
    /// Repository ID to discover execution features (entry-point call chains) in.
    pub repository_id: String,

    /// Maximum number of features to return, sorted by descending criticality
    /// (default: 20).
    #[serde(default = "default_features_limit")]
    pub limit: usize,
}

/// Input parameters for the get_feature tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFeatureInput {
    /// Entry-point symbol name (exact or substring) to retrieve the feature for.
    pub symbol: String,

    /// Restrict the lookup to a specific repository ID.
    pub repository_id: Option<String>,
}

/// Input parameters for the get_impacted_features tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactedFeaturesInput {
    /// Changed symbols. Every feature whose forward call chain includes at least
    /// one of these symbols is returned, sorted by descending criticality.
    pub symbols: Vec<String>,

    /// Restrict the analysis to a specific repository ID.
    pub repository_id: Option<String>,
}

/// Input parameters for the file_uses tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileUsesInput {
    /// Source repository (name or ID): the dependent side of the relationship.
    pub from: String,

    /// Target repository (name or ID): the dependency side of the relationship.
    pub to: String,
}

/// A file-level dependency relationship returned by the file_uses tool.
#[derive(Debug, Serialize)]
pub struct FileUsesResult {
    /// Resolved name of the source ("from") repository.
    pub from_repository: String,
    /// Resolved name of the target ("to") repository.
    pub to_repository: String,
    /// Directed file→file edges from the source repository into the target.
    pub edges: Vec<FileEdge>,
    /// Total number of edges returned.
    pub total: usize,
}

/// Input parameters for the list_clusters tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListClustersInput {
    /// Repository ID to detect architectural clusters in.
    pub repository_id: String,
}

/// Input parameters for the get_file_cluster tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileClusterInput {
    /// File path to locate within the repository's cluster graph.
    pub file_path: String,

    /// Repository ID the file belongs to.
    pub repository_id: String,
}

/// Input parameters for the architecture_overview tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArchitectureOverviewInput {
    /// Repository ID to summarise as a Markdown architecture table.
    pub repository_id: String,
}

/// Which graph the couplings analysis runs over.
fn default_coupling_level() -> String {
    "file".to_string()
}

/// Input parameters for the couplings tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CouplingsInput {
    /// Repository ID to analyse for coupling elements.
    pub repository_id: String,

    /// Which graph to analyse: "file" (file-dependency graph, the default) or
    /// "symbol" (symbol call graph).
    #[serde(default = "default_coupling_level")]
    pub level: String,
}

/// Input parameters for the channels tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsInput {
    /// Restrict matching to these repository IDs. Omit to match across every
    /// repository in the namespace.
    pub repository_ids: Option<Vec<String>>,

    /// Filter by protocol: "kafka", "http", "mqtt", "amqp", or "grpc".
    pub protocol: Option<String>,

    /// Drop edges whose confidence is below this threshold (0.0 to 1.0).
    pub min_confidence: Option<f32>,

    /// Glob patterns (`*`, `?`) excluding channels from matching and output,
    /// e.g. ["/health*"].
    #[serde(default)]
    pub exclude_channels: Vec<String>,

    /// Include endpoints from test files (test/, spec/, *-test.*, *.spec.*).
    /// Excluded by default, since test files rarely describe real traffic.
    #[serde(default)]
    pub include_tests: bool,
}

/// Input parameters for the list_symbol_clusters tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSymbolClustersInput {
    /// Repository ID to detect symbol communities in.
    pub repository_id: String,
}

/// Input parameters for the get_symbol_cluster tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSymbolClusterInput {
    /// Symbol to locate — a fully-qualified name or a bare short name
    /// (e.g. `authenticate` or `pkg/Auth#authenticate().`).
    pub symbol: String,

    /// Repository ID the symbol belongs to.
    pub repository_id: String,
}

/// Input parameters for the search_memory tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchMemoryInput {
    /// Natural-language query describing what to recall
    /// (e.g. "user's code style preferences", "how we fixed the flaky CI").
    pub query: String,

    /// Restrict to one memory kind: "preference", "experience", "skill", or
    /// "fact". Omit to search across all kinds.
    pub kind: Option<String>,

    /// Maximum number of results to return (default: 10, server cap: 100)
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Input parameters for the list_memories tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListMemoriesInput {
    /// Restrict to one memory kind: "preference", "experience", "skill", or
    /// "fact". Omit to list all kinds.
    pub kind: Option<String>,
}

/// Input parameters for the read_memory tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMemoryInput {
    /// A `memory://` node URI. Omit (or pass "memory://memory") to read the
    /// whole-memory rollup — the "read this first" summary of everything
    /// stored. Use "memory://sessions" to see stored sessions, or a specific
    /// "memory://sessions/<id>" to read one session's transcript.
    pub uri: Option<String>,
}

/// A virtual-filesystem node returned by read_memory
#[derive(Debug, Serialize)]
pub struct MemoryNodeOutput {
    /// The node's `memory://` URI
    pub uri: String,
    /// Node kind: memory, session, or resource
    pub kind: String,
    /// L0 — one-line abstract
    pub r#abstract: String,
    /// L1 — overview outline
    pub overview: String,
    /// L2 — full detail (e.g. a session transcript); empty for index nodes
    pub content: String,
    /// Child nodes (URI + abstract) when this node is a directory
    pub children: Vec<MemoryNodeChild>,
}

/// A child entry listed under a directory node
#[derive(Debug, Serialize)]
pub struct MemoryNodeChild {
    pub uri: String,
    pub kind: String,
    pub r#abstract: String,
}

/// A memory item returned by search_memory
#[derive(Debug, Serialize)]
pub struct MemorySearchResultOutput {
    /// Item ID (stable across updates)
    pub id: String,
    /// Memory kind: preference, experience, skill, or fact
    pub kind: String,
    /// Snake_case topic identifier, unique per kind
    pub name: String,
    /// Full Markdown content of the memory
    pub content: String,
    /// Fused relevance score (higher is better)
    pub score: f32,
    /// Unix timestamp of the last update
    pub updated_at: i64,
}

/// Parse an optional memory-kind filter, rejecting unknown values.
fn parse_kind_filter(kind: &Option<String>) -> Result<Option<MemoryKind>, McpError> {
    match kind {
        None => Ok(None),
        Some(k) => MemoryKind::parse(k).map(Some).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "Unknown memory kind '{k}' (expected preference, experience, skill, or fact)"
                ),
                None,
            )
        }),
    }
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

        let use_case = self.container.call_graph_use_case();

        let mut base_query = CallGraphQuery::new();
        if let Some(repo_id) = &input.repository_id {
            base_query = base_query.with_repository(repo_id.clone());
        }
        if let Some(limit) = input.limit {
            base_query = base_query.with_limit(limit as u32);
        }

        // Each arm returns (references, use_caller).
        // use_caller=true  → node.symbol = caller_symbol (who performs the action)
        // use_caller=false → node.symbol = callee_symbol (what is acted upon)
        let (references, use_caller) = match input.pattern {
            QueryPattern::CallersOf => {
                let refs = use_case
                    .find_callers(&input.target, &base_query)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, true)
            }
            QueryPattern::CalleesOf => {
                let refs = use_case
                    .find_callees(&input.target, &base_query)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, false)
            }
            QueryPattern::ImportsOf => {
                let q = base_query.with_reference_kind("import");
                let refs = use_case
                    .find_callees(&input.target, &q)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, false)
            }
            QueryPattern::ImportersOf => {
                let q = base_query.with_reference_kind("import");
                let refs = use_case
                    .find_callers(&input.target, &q)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, true)
            }
            QueryPattern::InheritorsOf => {
                // Halve the per-query limit so the combined result stays within the
                // requested bound before deduplication.
                let per_limit = input.limit.map(|n| ((n + 1) / 2) as u32);
                let q_inh = {
                    let q = base_query.clone().with_reference_kind("inheritance");
                    match per_limit {
                        Some(pl) => q.with_limit(pl),
                        None => q,
                    }
                };
                let q_imp = {
                    let q = base_query.clone().with_reference_kind("implementation");
                    match per_limit {
                        Some(pl) => q.with_limit(pl),
                        None => q,
                    }
                };
                let mut refs = use_case
                    .find_callers(&input.target, &q_inh)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                let mut refs2 =
                    use_case
                        .find_callers(&input.target, &q_imp)
                        .await
                        .map_err(|e| {
                            McpError::internal_error(format!("query_graph failed: {}", e), None)
                        })?;
                refs.append(&mut refs2);
                (refs, true)
            }
            QueryPattern::ChildrenOf => {
                let per_limit = input.limit.map(|n| ((n + 1) / 2) as u32);
                let q_inh = {
                    let q = base_query.clone().with_reference_kind("inheritance");
                    match per_limit {
                        Some(pl) => q.with_limit(pl),
                        None => q,
                    }
                };
                let q_imp = {
                    let q = base_query.clone().with_reference_kind("implementation");
                    match per_limit {
                        Some(pl) => q.with_limit(pl),
                        None => q,
                    }
                };
                let mut refs = use_case
                    .find_callees(&input.target, &q_inh)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                let mut refs2 =
                    use_case
                        .find_callees(&input.target, &q_imp)
                        .await
                        .map_err(|e| {
                            McpError::internal_error(format!("query_graph failed: {}", e), None)
                        })?;
                refs.append(&mut refs2);
                (refs, false)
            }
            QueryPattern::TestsFor => {
                let refs = use_case
                    .find_callers(&input.target, &base_query)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                let filtered: Vec<_> = refs
                    .into_iter()
                    .filter(|r| {
                        // Symbol-name heuristics (language-agnostic conventions).
                        let sym = r.caller_symbol().unwrap_or("").to_lowercase();
                        if sym.starts_with("test_")
                            || sym.ends_with("_test")
                            || sym.ends_with("_spec")
                        {
                            return true;
                        }
                        // Path heuristics: inspect components and file stem rather than
                        // doing a raw substring match to avoid false positives like
                        // "contest.rs" or "inspect.rs".
                        let path = Path::new(r.reference_file_path());
                        let test_dir = path.components().any(|c| {
                            if let std::path::Component::Normal(s) = c {
                                let s = s.to_string_lossy().to_lowercase();
                                matches!(s.as_str(), "test" | "tests" | "spec" | "specs")
                            } else {
                                false
                            }
                        });
                        if test_dir {
                            return true;
                        }
                        path.file_stem()
                            .map(|s| {
                                let s = s.to_string_lossy().to_lowercase();
                                s == "test"
                                    || s.starts_with("test_")
                                    || s.ends_with("_test")
                                    || s.ends_with("_spec")
                            })
                            .unwrap_or(false)
                    })
                    .collect();
                (filtered, true)
            }
            QueryPattern::FileSummary => {
                let refs = use_case
                    .find_by_file(&input.target, &base_query)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("query_graph failed: {}", e), None)
                    })?;
                (refs, false)
            }
        };

        // Deduplicate by symbol name, keeping the first reference site per unique symbol.
        // When use_caller is true, entries without a caller_symbol are dropped — a file
        // path is not a valid symbol and must not appear in GraphQueryNode.symbol.
        let mut seen: HashSet<String> = HashSet::new();
        let deduped = references.into_iter().filter_map(|r| {
            let symbol = if use_caller {
                r.caller_symbol()?.to_string()
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
        });
        let nodes: Vec<GraphQueryNode> = match input.limit {
            Some(n) => deduped.take(n).collect(),
            None => deduped.collect(),
        };

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

    /// List every indexed repository together with its file/chunk counts and
    /// per-language breakdown. Doubles as the "stats" view: sum the `file_count`
    /// and `chunk_count` fields across the returned repositories for aggregate
    /// totals. Use the returned repository IDs as the `repository_id` argument
    /// for the other tools.
    #[tool(name = "list_repositories")]
    async fn list_repositories(
        &self,
        _params: Parameters<ListRepositoriesInput>,
    ) -> Result<CallToolResult, McpError> {
        let use_case = self.container.list_use_case();
        let repos = use_case.execute().await.map_err(|e| {
            McpError::internal_error(format!("Failed to list repositories: {}", e), None)
        })?;

        let json = serde_json::to_string_pretty(&repos).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize repositories: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Discover execution features — named forward call chains rooted at
    /// entry-point symbols (symbols that call others but are never called within
    /// the repository) — and score each for criticality. Returns up to `limit`
    /// features sorted by descending criticality.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "list_features")]
    async fn list_features(
        &self,
        params: Parameters<ListFeaturesInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;
        let limit = input.limit.min(MAX_FEATURES_LIMIT);

        let use_case = self.container.execution_features_use_case();
        let features = use_case
            .list_features(&input.repository_id, limit)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Listing features failed: {}", e), None)
            })?;

        let json = serde_json::to_string_pretty(&features).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize features: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Retrieve a single execution feature by entry-point symbol name (exact or
    /// substring match). Returns `null` when the symbol cannot be resolved to an
    /// entry point in the call graph.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "get_feature")]
    async fn get_feature(
        &self,
        params: Parameters<GetFeatureInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let use_case = self.container.execution_features_use_case();
        let feature = use_case
            .get_feature(&input.symbol, input.repository_id.as_deref())
            .await
            .map_err(|e| McpError::internal_error(format!("Feature lookup failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&feature).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize feature: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Given a set of changed symbols, return every execution feature whose
    /// forward call chain includes at least one of them, sorted by descending
    /// criticality. Use this to assess which user-facing flows a change touches.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "get_impacted_features")]
    async fn get_impacted_features(
        &self,
        params: Parameters<ImpactedFeaturesInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let use_case = self.container.execution_features_use_case();
        let features = use_case
            .get_impacted_features(&input.symbols, input.repository_id.as_deref())
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Impacted features lookup failed: {}", e), None)
            })?;

        let json = serde_json::to_string_pretty(&features).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize features: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Show which files in one repository depend on files in another (or the same)
    /// repository. Resolves both `from` and `to` by repository name or ID, builds
    /// the cross-repository file-dependency graph, and returns the directed
    /// file→file edges flowing from the source into the target, each annotated
    /// with the referenced symbols and reference kinds.
    /// Requires the repositories to have been indexed with call-graph support.
    #[tool(name = "file_uses")]
    async fn file_uses(
        &self,
        params: Parameters<FileUsesInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let repos = self
            .container
            .list_use_case()
            .execute()
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Failed to list repositories: {}", e), None)
            })?;

        let resolve = |name_or_id: &str| -> Option<(String, String)> {
            repos
                .iter()
                .find(|r| r.id() == name_or_id)
                .or_else(|| {
                    repos
                        .iter()
                        .find(|r| r.name().eq_ignore_ascii_case(name_or_id))
                })
                .map(|r| (r.id().to_string(), r.name().to_string()))
        };

        let (from_id, from_name) = resolve(&input.from).ok_or_else(|| {
            McpError::invalid_params(format!("Repository not found: '{}'", input.from), None)
        })?;
        let (to_id, to_name) = resolve(&input.to).ok_or_else(|| {
            McpError::invalid_params(format!("Repository not found: '{}'", input.to), None)
        })?;

        let graph = self
            .container
            .file_graph_use_case()
            .build_graph(Some(&[from_id.clone(), to_id.clone()]), 1, true)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Failed to build file graph: {}", e), None)
            })?;

        let mut edges: Vec<FileEdge> = graph
            .edges
            .into_iter()
            .filter(|e| e.from_repo_id == from_id && e.to_repo_id == to_id)
            .collect();
        edges.sort_by(|a, b| {
            a.to_file
                .cmp(&b.to_file)
                .then(a.from_file.cmp(&b.from_file))
        });

        let total = edges.len();
        let result = FileUsesResult {
            from_repository: from_name,
            to_repository: to_name,
            edges,
            total,
        };

        let json = serde_json::to_string_pretty(&result).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize file uses: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Detect architectural clusters in a repository by running Leiden community
    /// detection over its file-dependency graph. Returns the clusters with their
    /// names, dominant language, cohesion score, and member files.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "list_clusters")]
    async fn list_clusters(
        &self,
        params: Parameters<ListClustersInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let use_case = self.container.cluster_detection_use_case();
        let cluster_graph = use_case
            .create_clusters(&input.repository_id)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Cluster detection failed: {}", e), None)
            })?;

        let json = serde_json::to_string_pretty(&cluster_graph).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize clusters: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Return the architectural cluster a specific file belongs to. Returns
    /// `null` when the file is not part of any detected cluster.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "get_file_cluster")]
    async fn get_file_cluster(
        &self,
        params: Parameters<GetFileClusterInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let use_case = self.container.cluster_detection_use_case();
        let cluster = use_case
            .cluster_for_file(&input.file_path, &input.repository_id)
            .await
            .map_err(|e| McpError::internal_error(format!("Cluster lookup failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&cluster).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize cluster: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Produce a high-level architecture overview of a repository as a Markdown
    /// table: one row per cluster with its file count, dominant language, and top
    /// inter-cluster dependencies.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "architecture_overview")]
    async fn architecture_overview(
        &self,
        params: Parameters<ArchitectureOverviewInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let use_case = self.container.cluster_detection_use_case();
        let overview = use_case
            .architecture_overview(&input.repository_id)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Architecture overview failed: {}", e), None)
            })?;

        Ok(CallToolResult::success(vec![Content::text(overview)]))
    }

    /// Find coupling elements: files/symbols or dependencies whose removal would
    /// split a Leiden community into two latent sub-blocks — the hub-like
    /// dependency / modularity-violation smell. Runs the filter-then-verify
    /// pipeline per community and reports, for each internally-fragile
    /// community, its two sub-blocks and the ablation-verified couplers holding
    /// them together (with split probabilities and the resolution range each
    /// controls). Set level to "file" or "symbol".
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "couplings")]
    async fn couplings(
        &self,
        params: Parameters<CouplingsInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let level = GraphLevel::parse(&input.level).map_err(|msg| {
            McpError::invalid_params(format!("invalid level '{}': {msg}", input.level), None)
        })?;

        let use_case = self.container.coupling_detection_use_case();
        let report = use_case
            .detect(&input.repository_id, level)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Coupling detection failed: {}", e), None)
            })?;

        let json = serde_json::to_string_pretty(&report).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize coupling report: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Show cross-service channel links between indexed repositories:
    /// producer/consumer call sites (Kafka topics, HTTP routes, MQTT topics)
    /// joined on their channel identifier. Returns matched producer→consumer
    /// edges plus dangling and unresolved endpoints, so you can answer "what
    /// connects these services" even when they share no symbols.
    /// Requires the repositories to have been indexed since channel
    /// extraction was introduced.
    #[tool(name = "channels")]
    async fn channels(
        &self,
        params: Parameters<ChannelsInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let protocol = match &input.protocol {
            Some(p) => Some(Protocol::parse(p).ok_or_else(|| {
                McpError::invalid_params(
                    format!("Unknown protocol '{p}' (expected kafka, http, mqtt, amqp, or grpc)"),
                    None,
                )
            })?),
            None => None,
        };

        let options = ChannelLinkOptions {
            protocol,
            min_confidence: input.min_confidence,
            exclude_channels: input.exclude_channels,
            include_tests: input.include_tests,
        };
        let report = self
            .container
            .channel_link_use_case()
            .link(input.repository_ids.as_deref(), &options)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Channel linking failed: {}", e), None)
            })?;

        let json = serde_json::to_string_pretty(&report).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize channel report: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Recall long-term memories extracted from previous assistant sessions:
    /// user preferences, reusable experiences, procedural skills, and project
    /// facts. Hybrid semantic + keyword search over the memory store. Call this
    /// at the start of a task to load relevant context — e.g. the user's code
    /// style preferences before writing code, or past experiences before
    /// debugging a familiar problem.
    /// Memories are created with `codesearch memory import <session.jsonl>`.
    #[tool(name = "search_memory")]
    async fn search_memory(
        &self,
        params: Parameters<SearchMemoryInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;
        let kind = parse_kind_filter(&input.kind)?;
        let limit = input.limit.min(MAX_LIMIT);

        let use_case = self.container.memory_search_use_case().map_err(|e| {
            McpError::internal_error(format!("Failed to open memory store: {}", e), None)
        })?;
        let results = use_case
            .execute(&input.query, kind, limit)
            .await
            .map_err(|e| McpError::internal_error(format!("Memory search failed: {}", e), None))?;

        let outputs: Vec<MemorySearchResultOutput> = results
            .into_iter()
            .map(|(item, score)| MemorySearchResultOutput {
                id: item.id().to_string(),
                kind: item.kind().as_str().to_string(),
                name: item.name().to_string(),
                content: item.content().to_string(),
                score,
                updated_at: item.updated_at(),
            })
            .collect();

        let json = serde_json::to_string_pretty(&outputs).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize memories: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// List stored long-term memories, newest first, optionally filtered by
    /// kind. Use kind="preference" at session start to load every known user
    /// preference at once; use search_memory instead when looking for something
    /// specific.
    #[tool(name = "list_memories")]
    async fn list_memories(
        &self,
        params: Parameters<ListMemoriesInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;
        let kind = parse_kind_filter(&input.kind)?;

        let repo = self.container.memory_repository().map_err(|e| {
            McpError::internal_error(format!("Failed to open memory store: {}", e), None)
        })?;
        let items = repo
            .list_items(kind)
            .await
            .map_err(|e| McpError::internal_error(format!("Memory listing failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&items).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize memories: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Read the memory virtual filesystem, level by level. Call this FIRST at
    /// the start of a task with no arguments (or uri="memory://memory") to get
    /// the whole-memory rollup — a single abstract + overview of everything
    /// known about the user and project — then drill in only where relevant.
    /// A directory URI (e.g. "memory://sessions") returns its children with
    /// one-line abstracts; a leaf URI (e.g. "memory://sessions/<id>") returns
    /// the node's full detail, such as a session transcript.
    #[tool(name = "read_memory")]
    async fn read_memory(
        &self,
        params: Parameters<ReadMemoryInput>,
    ) -> Result<CallToolResult, McpError> {
        use crate::application::MEMORY_ROOT_URI;

        let uri = params.0.uri.unwrap_or_else(|| MEMORY_ROOT_URI.to_string());

        let repo = self.container.memory_repository().map_err(|e| {
            McpError::internal_error(format!("Failed to open memory store: {}", e), None)
        })?;

        let node = repo
            .find_node(&uri)
            .await
            .map_err(|e| McpError::internal_error(format!("Memory read failed: {}", e), None))?;

        let children = repo
            .list_child_nodes(&uri)
            .await
            .map_err(|e| McpError::internal_error(format!("Memory read failed: {}", e), None))?
            .into_iter()
            .map(|c| MemoryNodeChild {
                uri: c.uri().to_string(),
                kind: c.kind().as_str().to_string(),
                r#abstract: c.abstract_().to_string(),
            })
            .collect::<Vec<_>>();

        let output = match node {
            Some(node) => MemoryNodeOutput {
                uri: node.uri().to_string(),
                kind: node.kind().as_str().to_string(),
                r#abstract: node.abstract_().to_string(),
                overview: node.overview().to_string(),
                content: node.content().to_string(),
                children,
            },
            // A directory URI (e.g. memory://sessions) may have no node record
            // of its own but still list children.
            None if !children.is_empty() => MemoryNodeOutput {
                uri: uri.clone(),
                kind: "directory".to_string(),
                r#abstract: String::new(),
                overview: String::new(),
                content: String::new(),
                children,
            },
            None => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "No memory node found at '{uri}'."
                ))]));
            }
        };

        let json = serde_json::to_string_pretty(&output).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize memory node: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Detect symbol communities in a repository by running Leiden community
    /// detection over its symbol call graph (one level finer than `list_clusters`,
    /// which works on files). Returns the communities with their names, dominant
    /// language, cohesion score, and member symbols — behavioural units that
    /// frequently cut across files.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "list_symbol_clusters")]
    async fn list_symbol_clusters(
        &self,
        params: Parameters<ListSymbolClustersInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let use_case = self.container.symbol_cluster_detection_use_case();
        let community_graph = use_case
            .detect_communities(&input.repository_id)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Symbol community detection failed: {}", e), None)
            })?;

        let json = serde_json::to_string_pretty(&community_graph).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize communities: {}", e), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Return the symbol community a specific symbol belongs to. Resolves the
    /// symbol by exact fully-qualified name, then boundary suffix, then substring.
    /// Returns `null` when the symbol is not part of any detected community.
    /// Requires the repository to have been indexed with call-graph support.
    #[tool(name = "get_symbol_cluster")]
    async fn get_symbol_cluster(
        &self,
        params: Parameters<GetSymbolClusterInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let use_case = self.container.symbol_cluster_detection_use_case();
        let community = use_case
            .community_for_symbol(&input.symbol, &input.repository_id)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Symbol community lookup failed: {}", e), None)
            })?;

        let json = serde_json::to_string_pretty(&community).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize community: {}", e), None)
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
                   imports_of, importers_of, inheritors_of, children_of, tests_for, file_summary\n\
                 • list_repositories — list indexed repositories with file/chunk counts (stats)\n\
                 • list_features — entry-point call chains scored by criticality\n\
                 • get_feature — a single execution feature by entry-point symbol\n\
                 • get_impacted_features — features whose call chain includes changed symbols\n\
                 • file_uses — which files in one repository depend on files in another\n\
                 • channels — cross-service producer→consumer links over Kafka/HTTP/MQTT channels\n\
                 • list_clusters — architectural (file-level) clusters via Leiden community detection\n\
                 • get_file_cluster — the cluster a given file belongs to\n\
                 • architecture_overview — Markdown table summarising clusters and dependencies\n\
                 • list_symbol_clusters — symbol-level communities via Leiden over the call graph\n\
                 • get_symbol_cluster — the symbol community a given symbol belongs to\n\
                 • search_memory — recall long-term memories (preferences, experiences, skills, \
                   facts) extracted from previous sessions\n\
                 • list_memories — list stored memories, optionally filtered by kind\n\
                 • read_memory — read the memory virtual filesystem; call with no args first for \
                   the whole-memory rollup, then drill into memory:// nodes (sessions, resources)"
                    .into(),
            ),
        }
    }
}
