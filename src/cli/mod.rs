use clap::{Subcommand, ValueEnum};

/// Default port for the MCP HTTP server started by `codesearch serve`.
pub const DEFAULT_MCP_PORT: u16 = 8677;

/// Default port for the REST/JSON management API started by `codesearch serve`.
pub const DEFAULT_MGMT_PORT: u16 = 8676;

/// Validates a namespace for use as a DuckDB schema name.
///
/// Schema names are always double-quoted in generated SQL, so almost any
/// character is safe. The one character that cannot appear is `"` itself,
/// because it would break the quoting even after standard `""` escaping in
/// the FTS PRAGMA argument (which is a SQL string, not a full SQL statement).
pub fn validate_namespace(s: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err("namespace must not be empty".to_string());
    }
    if s.contains('"') {
        return Err(format!(
            "namespace '{s}' contains '\"', which is not allowed in a namespace."
        ));
    }
    Ok(s.to_string())
}

/// Subcommands for the `features` command.
#[derive(Subcommand)]
pub enum FeaturesSubcommand {
    /// List all entry-point features for a repository, sorted by criticality.
    List {
        /// Repository ID or name. Omit to auto-detect from the current directory.
        #[arg(short, long)]
        repository: Option<String>,

        /// Maximum number of features to return.
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Output format: text, json, or vimgrep
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormat,
    },

    /// Show the execution feature for a single entry-point symbol.
    Get {
        /// Entry-point symbol name (exact or substring).
        symbol: String,

        /// Restrict lookup to a specific repository ID.
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text, json, or vimgrep
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormat,
    },

    /// Show features impacted by a set of changed symbols.
    Impacted {
        /// One or more changed symbol names.
        #[arg(required = true)]
        symbols: Vec<String>,

        /// Restrict lookup to a specific repository ID.
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text, json, or vimgrep
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
}

/// Output format for search results.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text (default)
    #[default]
    Text,
    /// JSON array of result objects
    Json,
    /// vimgrep-compatible format (file:line:col:text) for quickfix/Telescope
    Vimgrep,
}

/// Output format for cluster commands (text or json only).
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum OutputFormatTextJson {
    /// Human-readable text (default)
    #[default]
    Text,
    /// JSON array of result objects
    Json,
}

impl From<OutputFormatTextJson> for OutputFormat {
    fn from(f: OutputFormatTextJson) -> Self {
        match f {
            OutputFormatTextJson::Text => OutputFormat::Text,
            OutputFormatTextJson::Json => OutputFormat::Json,
        }
    }
}

/// Subcommands for the `clusters` command.
#[derive(Subcommand)]
pub enum ClustersSubcommand {
    /// List all clusters detected in the repository.
    List {
        /// Repository ID or name. Omit to auto-detect from the current directory.
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },

    /// Show the cluster that a specific file belongs to.
    Get {
        /// File path to look up (as indexed — relative to the repository root).
        file: String,

        /// Repository ID or name. Omit to auto-detect from the current directory.
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },

    /// Print a high-level Markdown architecture overview table.
    Overview {
        /// Repository ID or name. Omit to auto-detect from the current directory.
        #[arg(short, long)]
        repository: Option<String>,
    },
}

/// Subcommands for the `symbol-clusters` command — Leiden communities detected
/// over the symbol call graph (one level finer than file-level `clusters`).
#[derive(Subcommand)]
pub enum SymbolClustersSubcommand {
    /// List all symbol communities detected in the repository.
    List {
        /// Repository ID or name. Omit to auto-detect from the current directory.
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },

    /// Show the community that a specific symbol belongs to.
    Get {
        /// Symbol to look up — a fully-qualified name or a bare short name
        /// (e.g. `authenticate` or `pkg/Auth#authenticate().`).
        symbol: String,

        /// Repository ID or name. Omit to auto-detect from the current directory.
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },
}

/// Which graph level to visualize.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum VizLevel {
    /// File-dependency graph — architectural modules (default).
    #[default]
    File,
    /// Symbol call graph — behavioural communities.
    Symbol,
}

/// Output artifact for the `visualize` command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum VizFormat {
    /// Self-contained interactive vis-network page (default).
    #[default]
    Html,
    /// Static SVG image (embeds in Markdown/READMEs).
    Svg,
    /// Obsidian `.canvas` JSON.
    Canvas,
}

/// Initial mode for the interactive TUI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum TuiMode {
    /// Open in search mode (default).
    #[default]
    Search,
    /// Open in impact analysis mode.
    Impact,
    /// Open in context mode.
    Context,
}

/// Memory kind filter for `memory search` / `memory list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MemoryKindArg {
    Preference,
    Experience,
    Skill,
    Fact,
}

impl From<MemoryKindArg> for crate::domain::MemoryKind {
    fn from(arg: MemoryKindArg) -> Self {
        match arg {
            MemoryKindArg::Preference => crate::domain::MemoryKind::Preference,
            MemoryKindArg::Experience => crate::domain::MemoryKind::Experience,
            MemoryKindArg::Skill => crate::domain::MemoryKind::Skill,
            MemoryKindArg::Fact => crate::domain::MemoryKind::Fact,
        }
    }
}

/// Subcommands for the `memory` command — long-term memory extracted from
/// finished assistant sessions (stored in `memory.duckdb`, separate from the
/// code index).
#[derive(Subcommand)]
pub enum MemorySubcommand {
    /// Import a finished session transcript and extract memories from it.
    ///
    /// With no PATH, opens an interactive picker that discovers Claude Code,
    /// OpenCode, and Zed sessions on this machine — shown with their names, how
    /// long ago they ran, and a preview from the end of the conversation — and
    /// imports the ones you select.
    ///
    /// With a PATH, imports that transcript directly: a Claude Code session log
    /// (~/.claude/projects/<project>/<id>.jsonl) or a generic JSONL chat log
    /// ({"role": "...", "content": "..."} per line). Extraction calls the
    /// configured LLM — point ANTHROPIC_BASE_URL / ANTHROPIC_MODEL /
    /// ANTHROPIC_API_KEY (or the OPENAI_* equivalents with --llm open-ai) at a
    /// small model; extraction is a summarization-style task.
    Import {
        /// Path to a transcript file (JSONL). Omit to open the session picker.
        path: Option<String>,

        /// LLM provider for extraction: 'anthropic' (default) or 'open-ai'
        #[arg(long, value_enum, default_value = "anthropic")]
        llm: LlmTarget,

        /// Re-import even if this session was already imported.
        #[arg(short, long)]
        force: bool,
    },

    /// Search stored memories (hybrid semantic + keyword).
    Search {
        query: String,

        /// Maximum number of results.
        #[arg(long, default_value = "10")]
        num: usize,

        /// Restrict to one memory kind.
        #[arg(short, long, value_enum)]
        kind: Option<MemoryKindArg>,

        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },

    /// List stored memories, newest first.
    List {
        /// Restrict to one memory kind.
        #[arg(short, long, value_enum)]
        kind: Option<MemoryKindArg>,

        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },

    /// Show the full content of one memory item or virtual-filesystem node.
    Show {
        /// Memory item ID, a 'kind/name' item reference, or a 'memory://' node
        /// URI (e.g. 'memory://memory', 'memory://sessions/<id>').
        id: String,
    },

    /// Delete a memory item by ID.
    Delete {
        /// Memory item ID.
        id: String,
    },

    /// List imported sessions.
    Sessions {
        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },

    /// Add a resource (a file or a URL) to the memory virtual filesystem.
    ///
    /// Fetches the content (URLs and HTML are decluttered to Markdown via the
    /// `defuddle` CLI; plain files are read as-is), generates an L0 abstract +
    /// L1 overview, and stores it at 'memory://resources/<name>' with the full
    /// text as L2. Like `import`, this uses the configured LLM for the summary.
    Add {
        /// A local file path or an http(s):// URL.
        source: String,

        /// Name (slug) for the resource node; derived from the source when
        /// omitted. Reusing a name overwrites that resource.
        #[arg(long)]
        name: Option<String>,

        /// LLM provider for the summary: 'anthropic' (default) or 'open-ai'.
        #[arg(long, value_enum, default_value = "anthropic")]
        llm: LlmTarget,
    },

    /// Browse the memory virtual filesystem (L0/L1 abstracts).
    ///
    /// With no URI, lists the top-level roots (the whole-memory rollup and the
    /// sessions/resources directories). With a directory URI, lists its
    /// children with their one-line abstracts — the "read this first" view
    /// before drilling into a node with `memory show <uri>`.
    Tree {
        /// Directory URI to list (e.g. 'memory://sessions'). Omit for the root.
        uri: Option<String>,

        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },
}

/// Embedding backend to use for indexing and search.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum EmbeddingTarget {
    /// Bundled ONNX models from HuggingFace (offline-capable)
    #[default]
    Onnx,
    /// OpenAI-compatible /v1/embeddings endpoint (set OPENAI_BASE_URL to override)
    Api,
}

/// Provider to use for LLM calls (query expansion, explain, etc.).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LlmTarget {
    /// Anthropic-compatible /v1/messages (ANTHROPIC_BASE_URL, ANTHROPIC_MODEL)
    #[default]
    Anthropic,
    /// OpenAI-compatible /v1/chat/completions (OPENAI_BASE_URL, OPENAI_MODEL)
    OpenAi,
}

/// Reranking backend to use after retrieval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RerankingTarget {
    /// Bundled ONNX cross-encoder model (offline-capable)
    #[default]
    Onnx,
    /// LLM reranker via Anthropic-compatible /v1/messages (ANTHROPIC_BASE_URL, ANTHROPIC_MODEL)
    #[value(name = "api/anthropic")]
    ApiAnthropic,
    /// LLM reranker via OpenAI-compatible /v1/chat/completions (OPENAI_BASE_URL, OPENAI_MODEL)
    #[value(name = "api/openai")]
    ApiOpenAi,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a namespace with a fixed embedding configuration.
    ///
    /// A namespace's embedding setup is decided once, at creation, and
    /// inherited by every later `index` and `search` run against it — those
    /// commands read the stored configuration and never need embedding flags.
    /// Indexing into a namespace that was never created configures it with
    /// the defaults (ONNX, all-MiniLM-L6-v2, 384 dimensions).
    Create {
        /// Namespace to create (defaults to the global --namespace value)
        #[arg(value_parser = validate_namespace)]
        name: Option<String>,

        /// Embedding backend: 'onnx' (bundled, offline) or 'api' (OpenAI-compatible endpoint)
        #[arg(
            long,
            value_enum,
            default_value = "onnx",
            conflicts_with = "no_embeddings"
        )]
        embedding_target: EmbeddingTarget,

        /// Embedding model — HuggingFace ID for 'onnx', model name for 'api'
        /// (required for 'api'; defaults to all-MiniLM-L6-v2 for 'onnx')
        #[arg(long, conflicts_with = "no_embeddings")]
        embedding_model: Option<String>,

        /// Output dimensions of the embedding model
        #[arg(long, default_value = "384", conflicts_with = "no_embeddings")]
        embedding_dimensions: usize,

        /// Create the namespace without embeddings: indexing skips the embed
        /// stage (no model download or inference) and stores chunks, call
        /// graph, and BM25 index only; search uses the keyword and call-graph
        /// legs
        #[arg(long)]
        no_embeddings: bool,
    },

    /// Index a repository: parse, embed, and store its code for search
    Index {
        /// Path to the repository (or file) to index
        path: String,

        /// Namespace to index into (defaults to the global --namespace value)
        #[arg(short, long)]
        name: Option<String>,

        /// Force full re-index, ignoring cached file hashes
        #[arg(short, long)]
        force: bool,
    },

    /// Search indexed code by natural-language query (hybrid semantic + keyword)
    Search {
        /// Natural-language query describing what the code does
        query: String,

        #[arg(long, default_value = "10")]
        num: usize,

        #[arg(short, long)]
        min_score: Option<f32>,

        #[arg(short = 'L', long)]
        language: Option<Vec<String>>,

        #[arg(short, long)]
        repository: Option<Vec<String>>,

        /// Output format: text, json, or vimgrep (for Neovim/Telescope)
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormat,

        /// Disable keyword (BM25) search and use only semantic (vector) search
        #[arg(long = "no-text-search", default_value_t = true, action = clap::ArgAction::SetFalse)]
        text_search: bool,
    },

    /// List the repositories indexed in the current namespace
    List,

    /// Delete an indexed repository by its ID or path
    Delete {
        /// Repository ID or path to delete
        id_or_path: String,
    },

    /// Show index statistics (chunks, embeddings, call-graph size) for the namespace
    Stats,

    /// Show the blast radius of changing a symbol (BFS over the call graph)
    Impact {
        /// Symbol name or regex pattern (see --regex)
        symbol: String,

        /// Restrict analysis to a specific repository ID
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text or json
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormat,

        /// Treat SYMBOL as a literal regex; by default it is auto-wrapped as .*SYMBOL.*
        #[arg(long)]
        regex: bool,
    },

    /// Show callers (entry points → symbol) and callees (symbol → leaves) as an indented tree
    Context {
        /// Symbol name or regex pattern (see --regex)
        symbol: String,

        /// Restrict context to a specific repository ID
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text, json, or vimgrep
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormat,

        /// Treat SYMBOL as a literal regex; by default it is auto-wrapped as .*SYMBOL.*
        #[arg(long)]
        regex: bool,
    },

    /// LLM-driven explanation of a symbol's call flow, data flow, and business purpose
    Explain {
        /// Symbol name or regex pattern (see --regex)
        symbol: String,

        /// Restrict analysis to a specific repository ID
        #[arg(short, long)]
        repository: Option<String>,

        /// LLM provider: 'anthropic' (default) or 'open-ai'
        #[arg(long, value_enum, default_value = "anthropic")]
        llm: LlmTarget,

        /// Print each analyzed symbol and the source chunk sent to the LLM
        #[arg(long)]
        dump_symbols: bool,

        /// Treat SYMBOL as a literal regex; by default it is auto-wrapped as .*SYMBOL.*
        #[arg(long)]
        regex: bool,
    },

    /// Discover and score execution features rooted at entry-point symbols, ranked by criticality
    Features {
        #[command(subcommand)]
        subcommand: FeaturesSubcommand,
    },

    /// List cross-service channel links (Kafka topics, HTTP routes, MQTT
    /// topics) between the repositories indexed in the current namespace
    Channels {
        /// Restrict to specific repositories (name or ID, may be repeated).
        /// Omit to match across every repository in the namespace.
        #[arg(short, long)]
        repository: Option<Vec<String>>,

        /// Filter by protocol: kafka, http, mqtt, amqp, or grpc.
        #[arg(short, long)]
        protocol: Option<String>,

        /// Drop edges whose confidence is below this threshold (0.0–1.0).
        #[arg(long)]
        min_confidence: Option<f32>,

        /// Exclude channels matching this glob (may be repeated),
        /// e.g. --exclude-channel '/health*'.
        #[arg(long)]
        exclude_channel: Vec<String>,

        /// Include endpoints from test files (test/, spec/, *-test.*, *.spec.*).
        /// Excluded by default.
        #[arg(long)]
        include_tests: bool,

        /// Output format: text or json.
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormatTextJson,
    },

    /// List files in <from> that reference symbols defined in <to>
    Uses {
        /// Repository that is doing the using (the caller side).
        from: String,
        /// Repository being used (the dependency side).
        to: String,
    },

    /// Detect and explore architectural clusters in a repository's file dependency graph
    Clusters {
        #[command(subcommand)]
        subcommand: ClustersSubcommand,
    },

    /// Detect & query symbol communities (Leiden over the call graph).
    SymbolClusters {
        #[command(subcommand)]
        subcommand: SymbolClustersSubcommand,
    },

    /// Render a repository's Leiden communities as an HTML graph, SVG, or Obsidian canvas
    Visualize {
        /// Repository ID or name. Omit to auto-detect from the current directory.
        #[arg(short, long)]
        repository: Option<String>,

        /// Which graph to render: the file-dependency graph or the symbol call graph.
        #[arg(short, long, value_enum, default_value = "file")]
        level: VizLevel,

        /// Output artifact: html, svg, or canvas.
        #[arg(short = 'F', long, value_enum, default_value = "html")]
        format: VizFormat,

        /// Output path (defaults to ./codesearch-graph.<ext>)
        #[arg(short, long)]
        output: Option<String>,

        /// Collapse into a community meta-graph (one node per community); auto-applied above --node-limit
        #[arg(long)]
        aggregate: bool,

        /// Auto-aggregate when the graph has more than this many nodes.
        #[arg(long, default_value_t = 5000)]
        node_limit: usize,
    },

    /// Long-term memory: import finished sessions and search what was learned
    Memory {
        #[command(subcommand)]
        subcommand: MemorySubcommand,
    },

    /// Start MCP (Model Context Protocol) server for integration with AI tools
    Mcp {
        /// Run as HTTP server on specified port (e.g., --http 8080)
        #[arg(long)]
        http: Option<u16>,

        /// Bind to 0.0.0.0 instead of 127.0.0.1, exposing the server on all network interfaces
        #[arg(long)]
        public: bool,
    },

    /// Run both the MCP server (HTTP) and the REST/JSON management API together
    Serve {
        /// Port for the MCP HTTP server (Model Context Protocol endpoint at /mcp)
        #[arg(long, default_value_t = DEFAULT_MCP_PORT)]
        mcp_port: u16,

        /// Port for the REST/JSON management API server
        #[arg(long, default_value_t = DEFAULT_MGMT_PORT)]
        mgmt_port: u16,

        /// Bind to 0.0.0.0 instead of 127.0.0.1, exposing both servers on all
        /// network interfaces
        #[arg(long)]
        public: bool,
    },

    /// Launch the interactive TUI (search, impact, context in one terminal UI)
    Tui {
        /// Restrict all queries to a specific repository ID
        #[arg(short, long)]
        repository: Option<String>,

        /// Pre-populate the input box with this query and immediately dispatch it.
        #[arg(long)]
        query: Option<String>,

        /// Which mode to open the TUI in: 'search' (default), 'impact', or 'context'.
        #[arg(long, value_enum, default_value = "search")]
        mode: TuiMode,
    },
}
