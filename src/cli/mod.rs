use clap::{Subcommand, ValueEnum};

/// Output format for the file relationship graph.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum GraphFormat {
    /// Graphviz DOT language — pipe to `dot -Tsvg` to render (default).
    #[default]
    Dot,
    /// Mermaid diagram syntax — paste into any Mermaid renderer or GitHub markdown.
    Mermaid,
    /// JSON — structured data for programmatic consumption.
    Json,
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
    /// Bundled ONNX models downloaded from HuggingFace (default, offline-capable).
    #[default]
    Onnx,
    /// OpenAI-compatible `/v1/embeddings` API (e.g. LM Studio running locally).
    /// Set `OPENAI_BASE_URL` to override the default `http://localhost:1234`.
    Api,
}

/// Provider to use for LLM calls (query expansion, explain, etc.).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LlmTarget {
    /// Anthropic-compatible `/v1/messages` endpoint (default). Controlled by
    /// `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, and `ANTHROPIC_API_KEY`.
    #[default]
    Anthropic,
    /// OpenAI-compatible `/v1/chat/completions` endpoint. Controlled by
    /// `OPENAI_BASE_URL`, `OPENAI_MODEL`, and `OPENAI_API_KEY`.
    OpenAi,
}

/// Reranking backend to use after retrieval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RerankingTarget {
    /// Bundled ONNX cross-encoder model (default, offline-capable).
    #[default]
    Onnx,
    /// LLM reranker via Anthropic-compatible `/v1/messages` (e.g. LM Studio or
    /// Anthropic cloud). Controlled by `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`,
    /// and `ANTHROPIC_API_KEY`.
    #[value(name = "api/anthropic")]
    ApiAnthropic,
    /// LLM reranker via OpenAI-compatible `/v1/chat/completions` (e.g. LM
    /// Studio). Controlled by `OPENAI_BASE_URL`, `OPENAI_MODEL`, and
    /// `OPENAI_API_KEY`.
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

    /// Show the blast radius of changing a symbol (BFS over the call graph).
    Impact {
        /// Symbol name to analyse (e.g. "authenticate" or "MyStruct::new").
        /// When --regex is set, treated as a POSIX regular expression matched
        /// against all indexed fully-qualified symbol names.
        symbol: String,

        /// Restrict analysis to a specific repository ID
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text or json
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormat,

        /// Use SYMBOL as an explicit regex pattern without auto-wrapping.
        /// By default the symbol is automatically searched as `.*SYMBOL.*` so
        /// that "load" matches any FQN containing "load".  Pass --regex when
        /// you want to control anchoring yourself (e.g. "^MyNs/.*Service#get$").
        #[arg(long)]
        regex: bool,
    },

    /// Show full end-to-end call chain tree for a symbol: callers BFS (top-most
    /// entry points → symbol) and callees BFS (symbol → deepest callees), merged
    /// into one contiguous indented tree per caller chain.
    Context {
        /// Symbol name to look up (e.g. "authenticate" or "MyStruct::new").
        /// When --regex is set, treated as a POSIX regular expression matched
        /// against all indexed fully-qualified symbol names.
        symbol: String,

        /// Restrict context to a specific repository ID
        #[arg(short, long)]
        repository: Option<String>,

        /// Output format: text, json, or vimgrep
        #[arg(short = 'F', long, value_enum, default_value = "text")]
        format: OutputFormat,

        /// Use SYMBOL as an explicit regex pattern without auto-wrapping.
        /// By default the symbol is automatically searched as `.*SYMBOL.*` so
        /// that "load" matches any FQN containing "load".  Pass --regex when
        /// you want to control anchoring yourself (e.g. "^MyNs/.*Service#get$").
        #[arg(long)]
        regex: bool,
    },

    /// LLM-driven explanation of a symbol's complete call flow, data flow, and
    /// business purpose. Runs impact analysis then passes each affected symbol's
    /// source code to the configured LLM and returns a structured description.
    Explain {
        /// Symbol name to explain (e.g. "authenticate" or "MyStruct::new").
        /// When --regex is set, treated as a POSIX regular expression matched
        /// against all indexed fully-qualified symbol names.
        symbol: String,

        /// Restrict analysis to a specific repository ID
        #[arg(short, long)]
        repository: Option<String>,

        /// LLM backend to use for the explanation:
        ///   'anthropic' — /v1/messages (ANTHROPIC_BASE_URL, ANTHROPIC_MODEL, default).
        ///   'open-ai'   — /v1/chat/completions (OPENAI_BASE_URL, OPENAI_MODEL).
        #[arg(long, value_enum, default_value = "anthropic")]
        llm: LlmTarget,

        /// Print every analyzed symbol together with the source chunk that was
        /// sent to the LLM, after the explanation.
        #[arg(long)]
        dump_symbols: bool,

        /// Use SYMBOL as an explicit regex pattern without auto-wrapping.
        /// By default the symbol is automatically searched as `.*SYMBOL.*` so
        /// that "load" matches any FQN containing "load".  Pass --regex when
        /// you want to control anchoring yourself (e.g. "^MyNs/.*Service#get$").
        #[arg(long)]
        regex: bool,
    },

    /// Visualise file-level dependency relationships as a graph.
    ///
    /// Edges are drawn from a file that makes symbol references to the file that
    /// defines those symbols.  Repositories are shown as cluster boundaries;
    /// cross-repository edges are always included.  Use --repository to limit
    /// which repositories appear in the graph.
    ///
    /// Pipe DOT output through Graphviz to produce an image:
    ///   codesearch graph | dot -Tsvg -o deps.svg && open deps.svg
    Graph {
        /// Limit the graph to one or more repository IDs (may be repeated).
        /// When omitted every indexed repository is included.
        #[arg(short, long)]
        repository: Option<Vec<String>>,

        /// Output format: dot (Graphviz), mermaid, or json.
        #[arg(short = 'F', long, value_enum, default_value = "dot")]
        format: GraphFormat,

        /// Only include edges with at least this many symbol references.
        /// Raising the threshold reduces noise in dense codebases.
        #[arg(long, default_value = "1")]
        min_weight: usize,

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
