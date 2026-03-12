use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::AsyncWriteExt as _;

use anyhow::{Context, Result};

use crate::application::ChatClient;
use crate::cli::LlmTarget;
use crate::connector::adapter::{AnthropicClient, OpenAiChatClient};

use super::super::Container;

pub struct ExplainController<'a> {
    container: &'a Container,
}

impl<'a> ExplainController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn explain(
        &self,
        symbol: String,
        repository: Option<String>,
        llm: LlmTarget,
        dump_symbols: bool,
        is_regex: bool,
    ) -> Result<String> {
        let chat_client: Arc<dyn ChatClient> = match llm {
            LlmTarget::Anthropic => Arc::new(AnthropicClient::from_env()),
            LlmTarget::OpenAi => Arc::new(
                OpenAiChatClient::from_env()
                    .context("Failed to initialise OpenAI chat client for explain command")?,
            ),
        };

        let (token_tx, mut token_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let use_case = self.container.explain_use_case();
        let symbol_c = symbol.clone();
        let repo_c = repository.clone();
        let client_c = chat_client.clone();

        let use_case_handle = tokio::spawn(async move {
            use_case
                .execute_streaming(
                    &symbol_c,
                    repo_c.as_deref(),
                    client_c.as_ref(),
                    is_regex,
                    token_tx,
                )
                .await
        });

        // Stream tokens to stdout through the filter that converts XML section
        // tags to Markdown headings, strips bold markers, and removes any
        // surrounding ```xml … ``` code fence the LLM may emit.
        let mut filter = StreamingMarkdownFilter::new();
        let mut out = tokio::io::stdout();
        while let Some(token) = token_rx.recv().await {
            let converted = filter.process(&token);
            if !converted.is_empty() {
                out.write_all(converted.as_bytes())
                    .await
                    .context("failed to write token to stdout")?;
                out.flush().await.context("failed to flush stdout")?;
            }
        }
        // Flush all internal buffers and strip the closing fence if present.
        let remainder = filter.flush();
        if !remainder.is_empty() {
            out.write_all(remainder.as_bytes())
                .await
                .context("failed to write remainder to stdout")?;
        }

        let result: crate::application::ExplainResult = use_case_handle
            .await
            .context("explain task panicked")?
            .context("Explain use case failed")?;

        if !result.ambiguous_candidates.is_empty() {
            let mut output = format!(
                "'{}' matches {} symbols — please pick one and re-run with the full name:\n\n",
                result.root_symbol,
                result.ambiguous_candidates.len(),
            );
            for (i, candidate) in result.ambiguous_candidates.iter().enumerate() {
                output.push_str(&format!("  {}. {}\n", i + 1, candidate));
            }
            if result.is_regex {
                output.push_str(
                    "\nTip: narrow or anchor your regex to match a single symbol, \
                     e.g. use `^pattern` or `pattern$`.\n",
                );
            } else {
                output.push_str("\nRun with the full symbol name to explain a specific one.\n");
            }
            return Ok(output);
        }

        // Resolve repository IDs to human-readable names for the trailing section.
        let metadata_repo = self.container.metadata_repository();
        let mut repo_name_cache: HashMap<String, String> = HashMap::new();
        for (_symbol, repo_id, _file_path, _line, _src) in &result.symbol_sources {
            if !repo_name_cache.contains_key(repo_id.as_str()) {
                let name = metadata_repo
                    .find_by_id(repo_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.name().to_string())
                    .unwrap_or_else(|| repo_id.clone());
                repo_name_cache.insert(repo_id.clone(), name);
            }
        }

        // Build and write the trailing section directly to stdout so that
        // main.rs's println! does not duplicate the output.
        let mut trailing = format!(
            "\n\n---\nAnalysed {} symbols across {} call levels.\n\n",
            result.total_affected,
            result.max_depth_reached,
        );

        if dump_symbols {
            for (symbol, repo_id, file_path, line, src) in &result.symbol_sources {
                let repo_name = repo_name_cache
                    .get(repo_id.as_str())
                    .map(String::as_str)
                    .unwrap_or(repo_id.as_str());
                match src {
                    Some(s) => trailing.push_str(&format!(
                        "`{}` (`{}`) — `{}:{}`\n```\n{}\n```\n\n",
                        symbol, repo_name, file_path, line, s
                    )),
                    None => trailing.push_str(&format!(
                        "`{}` (`{}`) — `{}:{}` _(source not available)_\n\n",
                        symbol, repo_name, file_path, line
                    )),
                }
            }
        }

        if !result.symbol_sources.is_empty() {
            trailing.push_str("## Referenced files\n\n");
            for (symbol, repo_id, file_path, line, _src) in &result.symbol_sources {
                let repo_name = repo_name_cache
                    .get(repo_id.as_str())
                    .map(String::as_str)
                    .unwrap_or(repo_id.as_str());
                trailing.push_str(&format!(
                    "- {} {}:{} — {}\n",
                    repo_name, file_path, line, symbol
                ));
            }
            trailing.push('\n');
        }

        out.write_all(trailing.as_bytes())
            .await
            .context("failed to write trailing section to stdout")?;
        out.flush().await.context("failed to flush stdout")?;

        // Everything has been written to stdout directly; return empty so
        // main.rs's println! does not print a duplicate.
        Ok(String::new())
    }
}

// ---------------------------------------------------------------------------
// Streaming Markdown filter
// ---------------------------------------------------------------------------

/// XML section tags produced by the LLM mapped to their Markdown equivalents.
const TAG_MAP: &[(&str, &str)] = &[
    ("<purpose>", "## Purpose\n"),
    ("</purpose>", ""),
    ("<data_and_control_flow>", "\n## Data and control flow\n"),
    ("</data_and_control_flow>", ""),
    ("<business_feature>", "\n## Business feature\n"),
    ("</business_feature>", ""),
    (
        "<key_patterns_and_dependencies>",
        "\n## Key patterns and dependencies\n",
    ),
    ("</key_patterns_and_dependencies>", ""),
];

/// Opening code-fence variants the LLM may wrap its entire response in.
const OPENING_FENCES: &[&str] = &["```xml\n", "```xml\r\n"];

/// Bytes held back from emission to allow closing-fence detection at flush.
/// Must be >= the length of the longest expected closing-fence sequence.
const TAIL_RESERVE: usize = 8;

/// Converts a stream of raw LLM tokens into clean Markdown output.
///
/// Four transformations are applied in pipeline order, all designed to
/// introduce as little buffering as possible so the output streams with the
/// natural token-by-token cadence of the LLM:
///
/// 1. **XML → Markdown headings** — `<section>` / `</section>` tags are
///    replaced with `## Heading` lines.  Only bytes from `<` to `>` are held;
///    content between tags is released immediately.
/// 2. **Opening fence removal** — the optional ` ```xml\n ` wrapper Claude
///    sometimes emits is detected in the first few bytes and stripped.
/// 3. **Bold-marker stripping + blank-line collapsing** — implemented with a
///    tiny character-level state machine (one pending `*` at most) so output
///    is never delayed waiting for a full line.  Runs of more than one
///    consecutive blank line are collapsed to one.
/// 4. **Closing fence removal** — ` \n``` ` (and variants) at the very end
///    of the stream are stripped in [`StreamingMarkdownFilter::flush`].
struct StreamingMarkdownFilter {
    /// Holds bytes from the most recent unmatched `<` (stage 1).
    xml_buf: String,
    /// Accumulates the start of the stream to detect the opening fence (stage 2).
    preamble_buf: String,
    /// Set once the opening-fence check has been resolved.
    preamble_done: bool,
    /// A single pending `*` that may or may not be part of a `**` pair (stage 3).
    pending_star: bool,
    /// Number of consecutive `\n` characters emitted so far (stage 3).
    consecutive_newlines: usize,
    /// Holds the last [`TAIL_RESERVE`] bytes for closing-fence detection (stage 4).
    tail: String,
}

impl StreamingMarkdownFilter {
    fn new() -> Self {
        Self {
            xml_buf: String::new(),
            preamble_buf: String::new(),
            preamble_done: false,
            pending_star: false,
            consecutive_newlines: 0,
            tail: String::new(),
        }
    }

    /// Feed the next token and return text that is ready to emit to stdout.
    fn process(&mut self, token: &str) -> String {
        self.xml_buf.push_str(token);
        let xml_out = self.drain_xml();
        let pre_out = self.handle_preamble(xml_out);
        let filtered = self.filter_chars(pre_out);
        self.push_to_tail(filtered)
    }

    /// Flush all internal buffers at end-of-stream.
    ///
    /// Strips the closing code fence (` \n``` ` etc.) from whatever is left
    /// in the tail buffer before returning the final text.
    fn flush(mut self) -> String {
        // Flush the XML detection buffer (any partial plain text).
        let xml_rem = std::mem::take(&mut self.xml_buf);
        let pre_out = self.handle_preamble(xml_rem);
        let filtered = self.filter_chars(pre_out);
        self.tail.push_str(&filtered);

        // Flush the preamble buffer if we never accumulated enough chars.
        if !self.preamble_done {
            self.preamble_done = true;
            let rem = std::mem::take(&mut self.preamble_buf);
            self.tail.push_str(&rem);
        }

        // Flush any pending lone `*`.
        if self.pending_star {
            self.tail.push('*');
        }

        // Strip the closing code fence: find the last `\n``` ` and verify
        // that everything after it is whitespace before removing it.
        if let Some(pos) = self.tail.rfind("\n```") {
            let after = &self.tail[pos + 4..];
            if after.chars().all(|c| matches!(c, '\n' | '\r' | ' ')) {
                self.tail.truncate(pos);
            }
        }

        self.tail
    }

    // ── Stage 1: XML tag → Markdown heading ──────────────────────────────────

    fn drain_xml(&mut self) -> String {
        let mut out = String::new();
        loop {
            let Some(lt) = self.xml_buf.find('<') else {
                out.push_str(&self.xml_buf);
                self.xml_buf.clear();
                break;
            };
            out.push_str(&self.xml_buf[..lt]);
            self.xml_buf.drain(..lt);

            let Some(gt) = self.xml_buf.find('>') else {
                break; // Incomplete tag — wait for more tokens.
            };

            let replacement = TAG_MAP
                .iter()
                .find(|(t, _)| *t == &self.xml_buf[..=gt])
                .map(|(_, r)| r.to_string())
                .unwrap_or_else(|| self.xml_buf[..=gt].to_string());

            out.push_str(&replacement);
            self.xml_buf.drain(..=gt);
        }
        out
    }

    // ── Stage 2: Opening fence removal ───────────────────────────────────────

    fn handle_preamble(&mut self, text: String) -> String {
        if self.preamble_done {
            return text;
        }
        self.preamble_buf.push_str(&text);

        let max_fence = OPENING_FENCES.iter().map(|f| f.len()).max().unwrap_or(0);
        if self.preamble_buf.len() < max_fence {
            return String::new();
        }

        self.preamble_done = true;
        for &fence in OPENING_FENCES {
            if self.preamble_buf.starts_with(fence) {
                self.preamble_buf.drain(..fence.len());
                break;
            }
        }
        std::mem::take(&mut self.preamble_buf)
    }

    // ── Stage 3: Bold-marker stripping + blank-line collapsing ───────────────
    //
    // State machine with minimal buffering:
    //   - `pending_star`: a single `*` is held back until the next char
    //     determines whether it is half of a `**` bold marker (suppress both)
    //     or a lone `*` bullet/italic (emit it).
    //   - `consecutive_newlines`: counts how many `\n` we have emitted in a
    //     row; a third or more is suppressed to collapse blank lines.

    fn filter_chars(&mut self, text: String) -> String {
        let mut out = String::new();
        for ch in text.chars() {
            match ch {
                '*' => {
                    if self.pending_star {
                        // Second star: this is a `**` bold marker — suppress both.
                        self.pending_star = false;
                    } else {
                        self.pending_star = true;
                    }
                }
                '\n' => {
                    if self.pending_star {
                        out.push('*');
                        self.pending_star = false;
                    }
                    // Allow at most two consecutive newlines (one blank line).
                    if self.consecutive_newlines < 2 {
                        out.push('\n');
                        self.consecutive_newlines += 1;
                    }
                    // else: suppress the extra blank line
                }
                _ => {
                    if self.pending_star {
                        out.push('*');
                        self.pending_star = false;
                    }
                    self.consecutive_newlines = 0;
                    out.push(ch);
                }
            }
        }
        out
    }

    // ── Stage 4: Tail buffering ───────────────────────────────────────────────

    fn push_to_tail(&mut self, text: String) -> String {
        self.tail.push_str(&text);
        if self.tail.len() > TAIL_RESERVE {
            // Snap back to a char boundary so we never split a multi-byte character.
            let emit_len = self.tail.floor_char_boundary(self.tail.len() - TAIL_RESERVE);
            self.tail.drain(..emit_len).collect()
        } else {
            String::new()
        }
    }
}
