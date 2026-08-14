use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
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
use crate::domain::{FileEdge, GraphLevel, Protocol, SearchQuery};

use super::error::{ok_json, tool_error};
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

fn default_overview_top() -> usize {
    crate::application::DEFAULT_OVERVIEW_TOP
}

/// Input parameters for the overview tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OverviewInput {
    /// Repository name or ID to summarise. Omit to auto-detect the repository
    /// from the connected workspace.
    pub repository_id: Option<String>,

    /// Maximum number of rows kept in ranked sections (execution features);
    /// cluster and community lists are returned whole (default: 10).
    #[serde(default = "default_overview_top")]
    pub top: usize,
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
        let results = use_case.execute(query).await.map_err(tool_error)?;

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

        ok_json(outputs)
    }

    /// Analyse the blast radius of changing a symbol.
    /// Performs a BFS through the call graph to find every symbol that directly or
    /// transitively calls (or depends on) the given symbol, grouped by hop depth.
    /// Requires the repository to have been indexed with call-graph support.
    /// Returns `resolved: false` when the symbol is not present in the index; in
    /// that case `total_affected: 0` means "not indexed", not "no callers".
    #[tool(name = "analyze_impact")]
    async fn analyze_impact(
        &self,
        params: Parameters<ImpactToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let analysis = self
            .container
            .impact_use_case()
            .analyze(&input.symbol, input.repository_id.as_deref(), input.regex)
            .await
            .map_err(tool_error)?;

        ok_json(analysis)
    }

    /// Get the 360-degree context for a symbol: who calls it (callers) and what it
    /// calls (callees). Useful for understanding a symbol's role in the codebase
    /// before refactoring or debugging.
    /// Requires the repository to have been indexed with call-graph support.
    /// Returns `resolved: false` when the symbol is not present in the index; in
    /// that case empty caller/callee lists mean "not indexed", not "no callers".
    #[tool(name = "get_symbol_context")]
    async fn get_symbol_context(
        &self,
        params: Parameters<ContextToolInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let ctx = self
            .container
            .context_use_case()
            .get_context(&input.symbol, input.repository_id.as_deref(), input.regex)
            .await
            .map_err(tool_error)?;

        ok_json(ctx)
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
            QueryPattern::CallersOf => (
                use_case
                    .find_callers(&input.target, &base_query)
                    .await
                    .map_err(tool_error)?,
                true,
            ),
            QueryPattern::CalleesOf => (
                use_case
                    .find_callees(&input.target, &base_query)
                    .await
                    .map_err(tool_error)?,
                false,
            ),
            QueryPattern::ImportsOf => {
                let q = base_query.with_reference_kind("import");
                (
                    use_case
                        .find_callees(&input.target, &q)
                        .await
                        .map_err(tool_error)?,
                    false,
                )
            }
            QueryPattern::ImportersOf => {
                let q = base_query.with_reference_kind("import");
                (
                    use_case
                        .find_callers(&input.target, &q)
                        .await
                        .map_err(tool_error)?,
                    true,
                )
            }
            // Inheritance and implementation are two edge kinds describing one
            // relationship, so both arms union a query over each. They differ
            // only in direction: inheritors are the callers of the type,
            // children the callees.
            QueryPattern::InheritorsOf | QueryPattern::ChildrenOf => {
                // Full limit per kind; the dedup-and-take below bounds the union.
                // Halving it would return 5 of 10 when all edges are one kind.
                let kind_query = |kind: &str| base_query.clone().with_reference_kind(kind);
                let up = matches!(input.pattern, QueryPattern::InheritorsOf);

                let mut refs = Vec::new();
                for kind in ["inheritance", "implementation"] {
                    let q = kind_query(kind);
                    let mut found = if up {
                        use_case
                            .find_callers(&input.target, &q)
                            .await
                            .map_err(tool_error)?
                    } else {
                        use_case
                            .find_callees(&input.target, &q)
                            .await
                            .map_err(tool_error)?
                    };
                    refs.append(&mut found);
                }
                (refs, up)
            }
            QueryPattern::TestsFor => {
                let refs = use_case
                    .find_callers(&input.target, &base_query)
                    .await
                    .map_err(tool_error)?;
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
            QueryPattern::FileSummary => (
                use_case
                    .find_by_file(&input.target, &base_query)
                    .await
                    .map_err(tool_error)?,
                false,
            ),
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
        ok_json(GraphQueryResult {
            pattern: input.pattern,
            target: input.target,
            nodes,
            total,
        })
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
        let repos = self
            .container
            .list_use_case()
            .execute()
            .await
            .map_err(tool_error)?;

        ok_json(repos)
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

        let features = self
            .container
            .execution_features_use_case()
            .list_features(&input.repository_id, limit)
            .await
            .map_err(tool_error)?;

        ok_json(features)
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

        let feature = self
            .container
            .execution_features_use_case()
            .get_feature(&input.symbol, input.repository_id.as_deref())
            .await
            .map_err(tool_error)?;

        ok_json(feature)
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

        let features = self
            .container
            .execution_features_use_case()
            .get_impacted_features(&input.symbols, input.repository_id.as_deref())
            .await
            .map_err(tool_error)?;

        ok_json(features)
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

        let uses = self
            .container
            .file_graph_use_case()
            .uses_between(&input.from, &input.to)
            .await
            .map_err(tool_error)?;

        let total = uses.edges.len();
        ok_json(FileUsesResult {
            from_repository: uses.from_name,
            to_repository: uses.to_name,
            edges: uses.edges,
            total,
        })
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

        let cluster_graph = self
            .container
            .cluster_detection_use_case()
            .create_clusters(&input.repository_id)
            .await
            .map_err(tool_error)?;

        ok_json(cluster_graph)
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

        let cluster = self
            .container
            .cluster_detection_use_case()
            .cluster_for_file(&input.file_path, &input.repository_id)
            .await
            .map_err(tool_error)?;

        ok_json(cluster)
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

        let report = self
            .container
            .coupling_detection_use_case()
            .detect(&input.repository_id, level)
            .await
            .map_err(tool_error)?;

        ok_json(report)
    }

    /// One-shot repository dossier: index statistics, architectural modules
    /// (file-level Leiden clusters), behavioural symbol communities, coupling
    /// hotspots (god nodes), the most critical execution features, and
    /// cross-service channel links — assembled into a single JSON report.
    /// Every section is optional: a `null` section was disabled or could not be
    /// computed (its reason is listed under `skipped`), so the report degrades
    /// gracefully on a partially-indexed repository. Community `display_name`s
    /// carry whatever names were cached by earlier `clusters` / `symbol-clusters`
    /// runs; the LLM executive `summary` is not generated here and is always
    /// `null`. Use this to orient in a repository before drilling into a section
    /// with the more specific tools.
    #[tool(name = "overview")]
    async fn overview(
        &self,
        params: Parameters<OverviewInput>,
    ) -> Result<CallToolResult, McpError> {
        let input = params.0;

        let repository_id = self
            .container
            .resolve_repository_id(input.repository_id.as_deref())
            .await;

        // Channel links need both ends of an edge, so the join runs over the
        // current namespace's repositories (mirroring the `channels` command and
        // the CLI overview).
        let namespace = self.container.namespace();
        let channel_scope: Vec<String> = self
            .container
            .list_use_case()
            .execute()
            .await
            .map_err(tool_error)?
            .iter()
            .filter(|r| r.namespace() == Some(namespace))
            .map(|r| r.id().to_string())
            .collect();

        let options = crate::application::OverviewOptions {
            top: input.top,
            channel_scope,
            ..Default::default()
        };

        // No LLM enrichment over MCP: the report ships with cached community
        // names and no executive summary (the CLI presentation layer owns the
        // chat client). The caller can reason over the structured sections.
        let report = self
            .container
            .repository_overview_use_case()
            .execute(&repository_id, &options)
            .await
            .map_err(tool_error)?;

        ok_json(report)
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
            .map_err(tool_error)?;

        ok_json(report)
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

        let community_graph = self
            .container
            .symbol_cluster_detection_use_case()
            .detect_communities(&input.repository_id)
            .await
            .map_err(tool_error)?;

        ok_json(community_graph)
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

        let community = self
            .container
            .symbol_cluster_detection_use_case()
            .community_for_symbol(&input.symbol, &input.repository_id)
            .await
            .map_err(tool_error)?;

        ok_json(community)
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
                 • list_symbol_clusters — symbol-level communities via Leiden over the call graph\n\
                 • get_symbol_cluster — the symbol community a given symbol belongs to\n\
                 • search_memory — recall long-term memories (preferences, experiences, skills, \
                   facts) extracted from previous sessions\n\
                 • list_memories — list stored memories, optionally filtered by kind\n\
                 • read_memory — read the memory virtual filesystem; call with no args first for \
                   the whole-memory digest, then drill into memory:// nodes (sessions, resources)"
                    .into(),
            ),
        }
    }
}
