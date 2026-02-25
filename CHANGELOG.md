# Changelog

## Unreleased

### Features

* hybrid search enabled by default: every `search` query now runs both a semantic (vector) leg and a keyword (BM25-style LIKE) leg, then fuses them via Reciprocal Rank Fusion (RRF); use `--no-text-search` to opt back to pure semantic search
* add `--no-text-search` flag to the `search` command to disable the keyword leg and use only vector similarity

### Bug Fixes

* pre-rerank score filter (`>= 0.1`) no longer silently drops hybrid results whose RRF scores (~0.016–0.033) are below the threshold; the filter is now bypassed when text search is enabled
* LIKE escape character in DuckDB text search corrected from two-character `\\` to single-character `!`, fixing invalid SQL generated against DuckDB's `ESCAPE` clause
* `min_score` pruning in the semantic leg no longer runs before `rrf_fuse` in hybrid mode, preventing an asymmetric candidate pool; post-fusion filtering is now applied once by the caller
* misleading "Text search" label in the hybrid-path debug log renamed to "Hybrid search" to correctly reflect that both legs and RRF fusion are involved
* redundant `array_cosine_distance` call in `ORDER BY` replaced with `ORDER BY score DESC` alias, eliminating a duplicate per-row vector computation

### Tests

* comprehensive unit tests for `rrf_fuse`: empty inputs, rank-order scoring, dual-membership boost, `limit` truncation, and formula correctness
* unit tests for `SearchQuery.with_text_search` / `is_text_search`: default value, toggle behaviour, independence from other fields, and `summary()` output
* unit tests for `InMemoryVectorRepository` hybrid paths: cosine vs RRF score ranges, dual-leg ranking, post-fusion `min_score` filtering, no early pruning, empty-term fallback, and limit enforcement
* integration tests for end-to-end hybrid search: results returned with positive scores, keyword-matched chunk surfaces, special SQL characters (`%`, `_`, `!`) do not cause errors, and semantic-only baseline confirms the flag gates the BM25 leg

## [0.12.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.11.0...v0.12.0) (2026-02-25)


### Features

* filter out global search results with score &lt; 0.09 ([#85](https://github.com/ArtemisMucaj/codesearch/issues/85)) ([d3fd344](https://github.com/ArtemisMucaj/codesearch/commit/d3fd344a610f963df8e33457101fa79a3ba2984a))


### Bug Fixes

* bm25 wasn't executed ([#80](https://github.com/ArtemisMucaj/codesearch/issues/80)) ([456be08](https://github.com/ArtemisMucaj/codesearch/commit/456be0845ace0fa0773fab9c7ad909f6fb51076f))
* capture CommonJS require() bindings as import references in JS/TS ([283f0a2](https://github.com/ArtemisMucaj/codesearch/commit/283f0a2411549d6a196b78c25674b70fe94e420d))
* codesearch task spawn is always spanwn (not only on text editor focus) ([#76](https://github.com/ArtemisMucaj/codesearch/issues/76)) ([63f170c](https://github.com/ArtemisMucaj/codesearch/commit/63f170c671f4edbe9cd8a7087649773714326056))
* improve impact cmd output clarity and add line numbers ([#87](https://github.com/ArtemisMucaj/codesearch/issues/87)) ([bbbfd7e](https://github.com/ArtemisMucaj/codesearch/commit/bbbfd7eb091e85b1efdfb1300fe054592eb38311))

## [0.11.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.10.0...v0.11.0) (2026-02-24)


### Features

* log expanded query variants and reranking candidate counts ([#73](https://github.com/ArtemisMucaj/codesearch/issues/73)) ([a7819d3](https://github.com/ArtemisMucaj/codesearch/commit/a7819d35245a1edb11765052b32e8bc87af9db57))
* replace LIKE-based keyword search with real BM25 via DuckDB FTS ([#71](https://github.com/ArtemisMucaj/codesearch/issues/71)) ([3ef5dda](https://github.com/ArtemisMucaj/codesearch/commit/3ef5dda1ed0865b94772766c720326ce10cee4e3))

## [0.10.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.9.0...v0.10.0) (2026-02-24)


### Features

* add Zed editor integration ([#70](https://github.com/ArtemisMucaj/codesearch/issues/70)) ([c9c0717](https://github.com/ArtemisMucaj/codesearch/commit/c9c07179c053873cfdc5afb31db4fb3b887de733))


### Bug Fixes

* include anonymous callers in impact analysis results ([#66](https://github.com/ArtemisMucaj/codesearch/issues/66)) ([e513822](https://github.com/ArtemisMucaj/codesearch/commit/e51382220834aa1b4bdff3b9cb4058c488106078))

## [0.9.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.8.0...v0.9.0) (2026-02-23)


### Features

* add Kotlin language support ([#55](https://github.com/ArtemisMucaj/codesearch/issues/55)) ([8e5c70c](https://github.com/ArtemisMucaj/codesearch/commit/8e5c70c9f5f749f109da952dbb8571a3ca7fa871))
* add natural language query expansion with RRF multi-query fusion ([#56](https://github.com/ArtemisMucaj/codesearch/issues/56)) ([33210f1](https://github.com/ArtemisMucaj/codesearch/commit/33210f19915134be0b2fd264128cb7295532a769))

## [0.8.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.7.0...v0.8.0) (2026-02-22)


### Features

* add Swift language support ([#52](https://github.com/ArtemisMucaj/codesearch/issues/52)) ([c321aaa](https://github.com/ArtemisMucaj/codesearch/commit/c321aaaf825b74bc51eb01e39ef90476fb30e4c4))


### Bug Fixes

* add repository_id to ImpactNode for cross-repo impact visibility ([#53](https://github.com/ArtemisMucaj/codesearch/issues/53)) ([cc2673d](https://github.com/ArtemisMucaj/codesearch/commit/cc2673d3c1b72746db9c29f5b188b15307b53d99))

## [0.7.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.6.0...v0.7.0) (2026-02-22)


### Features

* add hybrid search, impact analysis, and symbol context ([#48](https://github.com/ArtemisMucaj/codesearch/issues/48)) ([479efcb](https://github.com/ArtemisMucaj/codesearch/commit/479efcbc8b955e057366e1e1a59b70837fbc0dd3))
* lower test file scores in search results ([#50](https://github.com/ArtemisMucaj/codesearch/issues/50)) ([a5ecb47](https://github.com/ArtemisMucaj/codesearch/commit/a5ecb4764b659ab1ab5de022f8f08126224f072f))


### Bug Fixes

* allow concurrent search commands by opening DuckDB in read-only mode ([#47](https://github.com/ArtemisMucaj/codesearch/issues/47)) ([f083212](https://github.com/ArtemisMucaj/codesearch/commit/f0832124c1b5c7cf5a6f659fda1dcd625791f9cc))

## [0.6.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.5.0...v0.6.0) (2026-02-16)


### Features

* add `--format` flag to search command with `text`, `json`, and `vimgrep` output modes ([3c87058](https://github.com/ArtemisMucaj/codesearch/commit/3c870588428f18b2006a32f737879dd68fc920a1))
* add call graph indexing to track symbol references ([ef4c6ec](https://github.com/ArtemisMucaj/codesearch/commit/ef4c6ec2c75a2b574449e6672182c93bb8b655c3))
* add skill for codesearch CLI ([#42](https://github.com/ArtemisMucaj/codesearch/issues/42)) ([e228bde](https://github.com/ArtemisMucaj/codesearch/commit/e228bde8d5b5cffcc10c6caa22c585dbc57c951d))
* add Telescope/Neovim integration for semantic code search ([#40](https://github.com/ArtemisMucaj/codesearch/issues/40)) ([3c87058](https://github.com/ArtemisMucaj/codesearch/commit/3c870588428f18b2006a32f737879dd68fc920a1))
* expose search as mcp server ([#38](https://github.com/ArtemisMucaj/codesearch/issues/38)) ([eee2dda](https://github.com/ArtemisMucaj/codesearch/commit/eee2dda06bf1a527dd31708811be87c72bf81409))


### Bug Fixes

* normalize Go/C++ imports and remove duplicate Go patterns ([471c137](https://github.com/ArtemisMucaj/codesearch/commit/471c137ff937619005f1bf666637f0b305cb087b))
* prioritize callee capture over type_ref in reference extraction ([8401f5f](https://github.com/ArtemisMucaj/codesearch/commit/8401f5f62a6ae3ad6571b45e512ed26c262a1e0e))
* remove duplicate Python pattern and filter primitive types ([729ba99](https://github.com/ArtemisMucaj/codesearch/commit/729ba998eb907e15df98bf970a0250ae37cf515e))


### Performance Improvements

* optimize enclosing scope lookup from O(R×D) to O(D+R) ([32b6a6d](https://github.com/ArtemisMucaj/codesearch/commit/32b6a6de3eb9aacc08742ec02609ddfcbd89a91b))
* optimize reranking ([29a2e22](https://github.com/ArtemisMucaj/codesearch/commit/29a2e2248d8831f273fbae59877eba9a1852a672))

## [0.5.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.4.0...v0.5.0) (2026-02-03)


### Features

* add duration metrics to indexing, search, and reranking logs ([3df778d](https://github.com/ArtemisMucaj/codesearch/commit/3df778d5ba6494ee0f7bed2a4571c258dd022bfb))
* add multi-language repository support ([2a6664a](https://github.com/ArtemisMucaj/codesearch/commit/2a6664a6989fa582fee3c5650e37c5e80cdd919e))
* add progress bar to indexing operations ([dbd6787](https://github.com/ArtemisMucaj/codesearch/commit/dbd67874b8539235e2155b6901a2e315d48a4473))


### Bug Fixes

* various small fixes to make the tool robust ([5b71513](https://github.com/ArtemisMucaj/codesearch/commit/5b715130b2b2fff36fe7cc1e85817fa92f57d537))

## [0.4.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.3.0...v0.4.0) (2026-02-01)


### Features

* support cpp ([dc5f023](https://github.com/ArtemisMucaj/codesearch/commit/dc5f0230eb3090f4963249ed6e7d070fea4d5050))

## [0.3.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.2.0...v0.3.0) (2026-01-31)


### Features

* incremental file indexing ([bf722eb](https://github.com/ArtemisMucaj/codesearch/commit/bf722eb1b48d0965af65cae31f3bbbe9c7116cf3))

## [0.2.0](https://github.com/ArtemisMucaj/codesearch/compare/v0.1.1...v0.2.0) (2026-01-30)


### Features

* add hcl, php treesitter languages ([04e6349](https://github.com/ArtemisMucaj/codesearch/commit/04e634953193602d43875c0638ecac2e148da39c))
* add hcl, php treesitter languages ([0968e04](https://github.com/ArtemisMucaj/codesearch/commit/0968e046dc151a517e1997a7001b3bd0dd47fe74))
* rerank results ([14753f8](https://github.com/ArtemisMucaj/codesearch/commit/14753f8d3a7a5314d40795d847adf2ef22664ace))
* rerank results ([2765a95](https://github.com/ArtemisMucaj/codesearch/commit/2765a959464ee87ab177b5fdc03ac2256336c452))
* support duckdb vector store ([6a80323](https://github.com/ArtemisMucaj/codesearch/commit/6a80323f1b1882a15594ba814575d50134ddb585))
* support duckdb vector store ([65381d0](https://github.com/ArtemisMucaj/codesearch/commit/65381d09696aeafe95482e328ddc3197acfb3752))

## [0.1.1](https://github.com/ArtemisMucaj/codesearch/compare/v0.1.0...v0.1.1) (2026-01-29)


### Bug Fixes

* release action workflow ([f7972bc](https://github.com/ArtemisMucaj/codesearch/commit/f7972bcebac3187501ab1ece2eab345fa8db341b))
* remove release job in rust.yml action ([e4bfb38](https://github.com/ArtemisMucaj/codesearch/commit/e4bfb384609d339ebc4772e443cfc6df6314a935))
