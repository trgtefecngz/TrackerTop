# Contributing to TrackerTop

Thanks for your interest in contributing to **TrackerTop**!

## Adding a New Language Extractor

TrackerTop's parser core is open and pluggable via `LanguageExtractor` and `LanguageRegistry`.
See [docs/ADDING_LANGUAGES.md](docs/ADDING_LANGUAGES.md) for the complete 3-step guide.

## Development Commands

```bash
# Run full test suite (with native GUI)
cargo test

# Run headless CLI/MCP tests only (fast, no GUI deps)
cargo test --no-default-features

# Build optimized release binary
cargo build --release
```

## Pull Request Guidelines

- Keep all code, comments, UI labels, and docs in English.
- Add an integration test for any new language extractor or MCP tool in the matching `tests/*.rs` file (`tests/languages.rs` for extractors, `tests/queries.rs` for query tools; shared helpers live in `tests/common/mod.rs`).
- Ensure `cargo test` passes with zero warnings.
