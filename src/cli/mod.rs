use clap::{Subcommand, ValueEnum};

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
    Index {
        path: String,

        #[arg(short, long)]
        name: Option<String>,

        /// Force full re-index, ignoring cached file hashes
        #[arg(short, long)]
        force: bool,
    },

    Search {
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

    List,

    Delete {
        id_or_path: String,
    },

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

    /// Start MCP (Model Context Protocol) server for integration with AI tools
    Mcp {
        /// Run as HTTP server on specified port (e.g., --http 8080)
        #[arg(long)]
        http: Option<u16>,

        /// Bind to 0.0.0.0 instead of 127.0.0.1, exposing the server on all network interfaces
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
