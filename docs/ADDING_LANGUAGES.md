# Adding a New Language to TrackerTop

TrackerTop is built around an **open, pluggable parser core** (`LanguageExtractor` + `LanguageRegistry`).
You can add support for any programming, shader, config, or scripting language (e.g. Python, TypeScript, Go, C++, GLSL, Zig) **without touching** the database, indexer, graph engine, MCP server, HTML report, or desktop GUI.

---

## 3-Step Guide

### Step 1: Implement `LanguageExtractor`

Create a new file `src/parser/<your_lang>.rs` and implement the `LanguageExtractor` trait:

```rust
use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, file_symbol_id, make_file_symbol, make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::Result;

pub struct PythonExtractor;

impl LanguageExtractor for PythonExtractor {
    fn id(&self) -> &'static str {
        "python"
    }

    fn display_name(&self) -> &'static str {
        "Python"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    fn color_hex(&self) -> &'static str {
        "#facc15"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        // Use tree-sitter or a lightweight deterministic parser to push:
        // - `Symbol` entries (functions, classes, methods, tests, etc.)
        // - `RawReference` entries (`EdgeKind::Imports`, `Calls`, `UsesType`, `Tests`, etc.)
        Ok(out)
    }
}
```

### Step 2: Register It in `LanguageRegistry`

In `src/parser/mod.rs`:
1. Export your module: `pub mod python;`
2. Add one line inside `LanguageRegistry::default_builtin()`:
   ```rust
   reg.register(Arc::new(python::PythonExtractor));
   ```

### Step 3: Add a Test

Add a test fixture in `tests/languages.rs` verifying that your extractor discovers symbols, builds edges, and appears in MCP queries.

---

## What You Get Automatically

Once registered in `LanguageRegistry`, your language immediately works across:
- Parallel incremental indexing (`blake3` + `rayon`)
- Cross-file edge resolution, PageRank God Nodes, and Community clustering
- All 9 MCP tools (`repo_map`, `task_context`, `file_outline`, `read_symbol`, `impact_analysis`, etc.)
- Exact BPE token-savings telemetry (`trackertop stats`)
- Native Desktop GUI graph colors, island layout, filters, and inspector
- Concise Markdown summary (`trackertop report`)
