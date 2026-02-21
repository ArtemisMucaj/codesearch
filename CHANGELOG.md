# Changelog

## Unreleased

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
