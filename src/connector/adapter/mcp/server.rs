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

use crate::application::CallGraphQuery;
use crate::connector::api::Container;
use crate::domain::{FileEdge, SearchQuery, VectorStore};

use super::tools::SearchResultOutput;

/// Server-side maximum for the number of results a single search can return.
const MAX_LIMIT: usize = 100;

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

/// Input parameters for the index_repository tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IndexRepositoryInput {
    /// Filesystem path to the repository to index.
    pub path: String,

    /// Optional human-readable name (defaults to the directory name).
    pub name: Option<String>,

    /// When true, delete any existing index for this path and re-index from
    /// scratch. Defaults to false (incremental indexing).
    #[serde(default)]
    pub force: bool,
}

/// Result returned by the index_repository tool.
#[derive(Debug, Serialize)]
pub struct IndexRepositoryResult {
    /// Stable repository ID.
    pub id: String,
    /// Repository name.
    pub name: String,
    /// Absolute path that was indexed.
    pub path: String,
    /// Number of files indexed.
    pub file_count: u64,
    /// Number of code chunks produced.
    pub chunk_count: u64,
    /// Per-language file counts.
    pub languages: std::collections::HashMap<String, u64>,
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

        let use_case = self.container.execution_features_use_case();
        let features = use_case
            .list_features(&input.repository_id, input.limit)
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

    /// Index (or incrementally re-index) a repository at the given filesystem
    /// path so its code becomes searchable and its call graph is built. Set
    /// `force=true` to delete any existing index for the path and re-index from
    /// scratch. Returns the resulting repository's ID, file/chunk counts, and
    /// language breakdown. This is a heavy, long-running operation.
    #[tool(name = "index_repository")]
    async fn index_repository(
        &self,
        params: Parameters<IndexRepositoryInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        // Mirror the CLI IndexController: pick the vector store and namespace
        // based on how the container was configured.
        let (store, namespace) = if self.container.memory_storage() {
            (VectorStore::InMemory, None)
        } else {
            (
                VectorStore::DuckDb,
                Some(self.container.namespace().to_string()),
            )
        };

        let use_case = self.container.index_use_case();
        let repo = use_case
            .execute(
                &input.path,
                input.name.as_deref(),
                store,
                namespace,
                input.force,
            )
            .await
            .map_err(|e| McpError::internal_error(format!("Indexing failed: {}", e), None))?;

        let languages = repo
            .languages()
            .iter()
            .map(|(lang, stats)| (lang.clone(), stats.file_count))
            .collect();

        let result = IndexRepositoryResult {
            id: repo.id().to_string(),
            name: repo.name().to_string(),
            path: repo.path().to_string(),
            file_count: repo.file_count(),
            chunk_count: repo.chunk_count(),
            languages,
        };

        let json = serde_json::to_string_pretty(&result).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize index result: {}", e), None)
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
                 • list_clusters — architectural clusters via Leiden community detection\n\
                 • get_file_cluster — the cluster a given file belongs to\n\
                 • architecture_overview — Markdown table summarising clusters and dependencies\n\
                 • index_repository — index or re-index a repository at a filesystem path"
                    .into(),
            ),
        }
    }
}
