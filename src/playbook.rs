//! `project_playbook`: how to build, test, lint and format this project, and which
//! conventions and agent instructions apply, read from the project's own files.
//!
//! Every item says where it came from. `detected` means the project declares it (a script,
//! a CI step, a config file); `guessed` means it is the ecosystem default for a detected
//! toolchain and has not been confirmed by the project.

use crate::checks::detect_commands;
use std::path::Path;

/// One playbook line: text plus its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub source: String,
    pub detected: bool,
}

impl Item {
    fn detected(text: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            source: source.into(),
            detected: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Playbook {
    pub commands: Vec<Item>,
    pub ci_steps: Vec<Item>,
    pub toolchain: Vec<Item>,
    pub conventions: Vec<Item>,
    pub agent_docs: Vec<String>,
    pub checked: Vec<String>,
    /// Workflows that ship rather than check (release/deploy/publish), named only.
    pub release_workflows: Vec<String>,
    /// CI `run:` lines that are plain shell (variables, copies, loops), not tool commands.
    pub ci_other_lines: usize,
}

/// Whether a CI shell line invokes a build/test/lint tool (as opposed to plumbing like
/// variable assignments, copies and loops).
fn is_tool_command(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "cargo"
            | "rustup"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "bun"
            | "node"
            | "deno"
            | "go"
            | "golangci-lint"
            | "python"
            | "python3"
            | "pip"
            | "pytest"
            | "ruff"
            | "mypy"
            | "tox"
            | "uv"
            | "poetry"
            | "make"
            | "just"
            | "mvn"
            | "gradle"
            | "./gradlew"
            | "dotnet"
            | "tsc"
            | "eslint"
            | "prettier"
            | "biome"
            | "shellcheck"
            | "cmake"
            | "ctest"
            | "bazel"
    )
}

fn read(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// Value of `key = "value"` / `key = 'value'` / `key = value` on its own line in a
/// TOML-like file (first match; no section awareness beyond what callers check).
fn toml_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix(key)?.trim_start();
        let v = rest.strip_prefix('=')?.trim();
        let v = v.split('#').next().unwrap_or(v).trim();
        Some(v.trim_matches(|c| c == '"' || c == '\'').to_string())
    })
}

/// `run:` commands of GitHub Actions workflows, with their job names. Handles single-line
/// `run: cmd` and block scalars `run: |` / `run: >`.
pub fn workflow_run_steps(text: &str) -> Vec<(String, String)> {
    let mut steps = Vec::new();
    let mut job = String::new();
    let mut in_jobs = false;
    let mut jobs_indent = 0usize;
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        if t == "jobs:" && indent == 0 {
            in_jobs = true;
            jobs_indent = usize::MAX;
            i += 1;
            continue;
        }
        if in_jobs && indent == 0 {
            in_jobs = false;
        }
        if in_jobs && t.ends_with(':') && !t.starts_with('-') && indent <= jobs_indent {
            jobs_indent = indent;
            job = t.trim_end_matches(':').to_string();
        }
        let body = t.strip_prefix("- ").unwrap_or(t);
        if let Some(cmd) = body.strip_prefix("run:") {
            let cmd = cmd.trim();
            if cmd == "|" || cmd == ">" || cmd.starts_with("|") || cmd.starts_with(">") {
                let mut block = Vec::new();
                let mut j = i + 1;
                while j < lines.len() {
                    let l = lines[j];
                    let ind = l.len() - l.trim_start().len();
                    if !l.trim().is_empty() && ind <= indent {
                        break;
                    }
                    if !l.trim().is_empty() {
                        block.push(l.trim().to_string());
                    }
                    j += 1;
                }
                for b in block {
                    steps.push((job.clone(), b));
                }
                i = j;
                continue;
            }
            if !cmd.is_empty() {
                steps.push((
                    job.clone(),
                    cmd.trim_matches(|c| c == '"' || c == '\'').to_string(),
                ));
            }
        }
        i += 1;
    }
    steps
}

/// Target names of a Makefile or justfile (`name:` at column 0, excluding variables).
fn make_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for l in text.lines() {
        if l.starts_with(['\t', ' ', '#', '.']) {
            continue;
        }
        if let Some((name, rest)) = l.split_once(':')
            && !rest.starts_with('=')
            && !name.contains('=')
            && !name.contains('$')
        {
            let name = name.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() && !targets.contains(&name) {
                targets.push(name);
            }
        }
    }
    targets
}

pub fn build_playbook(root: &Path) -> Playbook {
    let mut pb = Playbook::default();

    for c in detect_commands(root) {
        pb.commands.push(Item {
            text: format!("{}: `{}`", c.kind.as_str(), c.display(&[])),
            source: c.source.clone(),
            detected: c.detected,
        });
    }

    // CI workflows: the commands CI actually runs are the strongest evidence
    let wf_dir = root.join(".github/workflows");
    pb.checked.push(".github/workflows/*.yml".to_string());
    if let Ok(entries) = std::fs::read_dir(&wf_dir) {
        let mut files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "yml" || e == "yaml"))
            .collect();
        files.sort();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let name = f
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let lower = name.to_ascii_lowercase();
            if ["release", "deploy", "publish"]
                .iter()
                .any(|w| lower.contains(w))
            {
                pb.release_workflows.push(name);
                continue;
            }
            for (job, cmd) in workflow_run_steps(&text) {
                if !is_tool_command(&cmd) {
                    pb.ci_other_lines += 1;
                    continue;
                }
                let job = if job.is_empty() {
                    String::new()
                } else {
                    format!(" `{job}`")
                };
                // The same command in several jobs is listed once with all its jobs
                if let Some(existing) = pb.ci_steps.iter_mut().find(|s| s.text == cmd) {
                    if !existing.source.contains(&job) {
                        existing.source.push_str(&job);
                    }
                } else {
                    pb.ci_steps.push(Item::detected(
                        cmd,
                        format!(".github/workflows/{name} job{job}"),
                    ));
                }
            }
        }
    }
    // A default command that CI runs is confirmed, not guessed
    for c in pb.commands.iter_mut().filter(|c| !c.detected) {
        let cmd = c
            .text
            .split_once('`')
            .map(|(_, r)| r.trim_end_matches('`'))
            .unwrap_or("");
        let head: Vec<&str> = cmd.split_whitespace().take(2).collect();
        if head.len() == 2
            && pb.ci_steps.iter().any(|s| {
                let mut w = s.text.split_whitespace();
                w.next() == Some(head[0]) && w.next() == Some(head[1])
            })
        {
            c.detected = true;
            c.source.push_str(", confirmed by CI");
        }
    }

    // Toolchains and versions
    pb.checked.extend(
        [
            "Cargo.toml",
            "rust-toolchain(.toml)",
            "package.json",
            ".nvmrc",
            "go.mod",
            "pyproject.toml",
            ".python-version",
        ]
        .map(String::from),
    );
    if let Some(cargo) = read(root, "Cargo.toml") {
        if let Some(ed) = toml_value(&cargo, "edition") {
            pb.toolchain
                .push(Item::detected(format!("Rust edition {ed}"), "Cargo.toml"));
        }
        if let Some(msrv) = toml_value(&cargo, "rust-version") {
            pb.toolchain.push(Item::detected(
                format!("minimum Rust {msrv}"),
                "Cargo.toml rust-version",
            ));
        }
        if cargo.contains("[workspace]") {
            pb.toolchain
                .push(Item::detected("Cargo workspace", "Cargo.toml [workspace]"));
        }
        if cargo.contains("[features]") {
            pb.conventions.push(Item::detected(
                "has Cargo features: also check `--no-default-features` / `--all-features` builds if CI does",
                "Cargo.toml [features]",
            ));
        }
    }
    for f in ["rust-toolchain.toml", "rust-toolchain"] {
        if let Some(t) = read(root, f) {
            let ch = toml_value(&t, "channel").unwrap_or_else(|| t.trim().to_string());
            pb.toolchain
                .push(Item::detected(format!("Rust toolchain `{ch}`"), f));
        }
    }
    if let Some(pkg) = read(root, "package.json")
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg)
    {
        if let Some(node) = v.pointer("/engines/node").and_then(|n| n.as_str()) {
            pb.toolchain.push(Item::detected(
                format!("Node {node}"),
                "package.json engines.node",
            ));
        }
        if let Some(pm) = v.get("packageManager").and_then(|n| n.as_str()) {
            pb.toolchain.push(Item::detected(
                format!("package manager {pm}"),
                "package.json packageManager",
            ));
        }
        if v.get("type").and_then(|t| t.as_str()) == Some("module") {
            pb.conventions.push(Item::detected(
                "ES modules (`\"type\": \"module\"`)",
                "package.json",
            ));
        }
    }
    if let Some(n) = read(root, ".nvmrc") {
        pb.toolchain
            .push(Item::detected(format!("Node {}", n.trim()), ".nvmrc"));
    }
    if let Some(gomod) = read(root, "go.mod")
        && let Some(v) = gomod
            .lines()
            .find_map(|l| l.trim().strip_prefix("go ").map(|v| v.trim().to_string()))
    {
        pb.toolchain
            .push(Item::detected(format!("Go {v}"), "go.mod"));
    }
    if let Some(py) = read(root, "pyproject.toml")
        && let Some(v) = toml_value(&py, "requires-python")
    {
        pb.toolchain.push(Item::detected(
            format!("Python {v}"),
            "pyproject.toml requires-python",
        ));
    }
    if let Some(v) = read(root, ".python-version") {
        pb.toolchain.push(Item::detected(
            format!("Python {}", v.trim()),
            ".python-version",
        ));
    }

    // Formatting / lint configuration and task runners
    for (file, what) in [
        ("rustfmt.toml", "rustfmt config"),
        (".rustfmt.toml", "rustfmt config"),
        ("clippy.toml", "clippy config"),
        (".editorconfig", "editor config (indentation, line endings)"),
        (".prettierrc", "Prettier config"),
        (".prettierrc.json", "Prettier config"),
        ("prettier.config.js", "Prettier config"),
        ("eslint.config.js", "ESLint flat config"),
        ("eslint.config.mjs", "ESLint flat config"),
        (".eslintrc.json", "ESLint config"),
        (".eslintrc.js", "ESLint config"),
        ("biome.json", "Biome config"),
        ("tsconfig.json", "TypeScript config"),
        ("ruff.toml", "Ruff config"),
        (
            ".pre-commit-config.yaml",
            "pre-commit hooks: run them before committing",
        ),
        ("CONTRIBUTING.md", "contribution guide"),
    ] {
        pb.checked.push(file.to_string());
        if root.join(file).is_file() {
            pb.conventions.push(Item::detected(what, file));
        }
    }
    for file in ["Makefile", "justfile", "Justfile"] {
        pb.checked.push(file.to_string());
        if let Some(text) = read(root, file) {
            let targets = make_targets(&text);
            if !targets.is_empty() {
                let runner = if file == "Makefile" { "make" } else { "just" };
                pb.commands.push(Item::detected(
                    format!("{runner} targets: {}", targets.join(", ")),
                    file,
                ));
            }
        }
    }

    // Instructions written for agents: read these before working
    for file in [
        "CLAUDE.md",
        "AGENTS.md",
        ".cursorrules",
        ".github/copilot-instructions.md",
        "GEMINI.md",
    ] {
        pb.checked.push(file.to_string());
        if root.join(file).is_file() {
            pb.agent_docs.push(file.to_string());
        }
    }
    pb
}

pub fn render(pb: &Playbook) -> String {
    fn section(out: &mut String, title: &str, items: &[Item]) {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("\n## {title}\n"));
        for it in items {
            out.push_str(&format!(
                "- {} [{} from {}]\n",
                it.text,
                if it.detected { "detected" } else { "guessed" },
                it.source
            ));
        }
    }
    let mut out = String::from("# PROJECT PLAYBOOK\n");
    if !pb.agent_docs.is_empty() {
        out.push_str(&format!(
            "READ FIRST (instructions for agents): {}\n",
            pb.agent_docs.join(", ")
        ));
    }
    section(
        &mut out,
        "Commands (run them with `run_checks`)",
        &pb.commands,
    );
    section(
        &mut out,
        "What CI runs (the bar a change must pass)",
        &pb.ci_steps,
    );
    if pb.ci_other_lines > 0 {
        out.push_str(&format!(
            "({} other CI shell lines are plumbing, not tool commands, and are not listed)\n",
            pb.ci_other_lines
        ));
    }
    if !pb.release_workflows.is_empty() {
        out.push_str(&format!(
            "Release/deploy workflows (not checks, not listed): {}\n",
            pb.release_workflows.join(", ")
        ));
    }
    section(&mut out, "Toolchain", &pb.toolchain);
    section(&mut out, "Conventions and config", &pb.conventions);
    if pb.commands.is_empty() && pb.ci_steps.is_empty() && pb.toolchain.is_empty() {
        out.push_str("\nNOTHING DETECTED: no known build/test configuration was found.\n");
    }
    out.push_str(&format!(
        "\nCOVERAGE: checked {}. `guessed` items are ecosystem defaults the project has not \
         confirmed; prefer what CI runs.\n",
        pb.checked.join(", ")
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_steps_single_line_and_block() {
        let wf = "name: CI\non: [push]\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: cargo fmt --check\n      - name: clippy\n        run: |\n          cargo clippy --all-targets -- -D warnings\n          cargo clippy --no-default-features -- -D warnings\n  test:\n    steps:\n      - run: \"cargo test\"\n";
        assert_eq!(
            workflow_run_steps(wf),
            vec![
                ("lint".to_string(), "cargo fmt --check".to_string()),
                (
                    "lint".to_string(),
                    "cargo clippy --all-targets -- -D warnings".to_string()
                ),
                (
                    "lint".to_string(),
                    "cargo clippy --no-default-features -- -D warnings".to_string()
                ),
                ("test".to_string(), "cargo test".to_string()),
            ]
        );
    }

    #[test]
    fn make_targets_skip_variables_and_recipes() {
        let mk = "CC := gcc\nVAR = 1\n.PHONY: all\nall: build\nbuild:\n\tcargo build\ntest: build\n\tcargo test\n";
        assert_eq!(make_targets(mk), vec!["all", "build", "test"]);
    }

    #[test]
    fn playbook_marks_provenance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\nrust-version = \"1.89\"\n\n[features]\ngui = []\n",
        )
        .unwrap();
        std::fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        std::fs::write(
            root.join(".github/workflows/ci.yml"),
            "jobs:\n  test:\n    steps:\n      - run: cargo test --all-features\n",
        )
        .unwrap();
        std::fs::write(root.join("AGENTS.md"), "# rules\n").unwrap();
        std::fs::write(root.join("rustfmt.toml"), "max_width = 100\n").unwrap();
        let text = render(&build_playbook(root));
        assert!(
            text.contains("READ FIRST (instructions for agents): AGENTS.md"),
            "{text}"
        );
        assert!(
            text.contains("- test: `cargo test` [detected from Cargo.toml]"),
            "{text}"
        );
        assert!(
            text.contains(
                "- lint: `cargo clippy --all-targets -- -D warnings` [guessed from Cargo.toml]"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "- cargo test --all-features [detected from .github/workflows/ci.yml job `test`]"
            ),
            "{text}"
        );
        assert!(text.contains("Rust edition 2024"), "{text}");
        assert!(text.contains("minimum Rust 1.89"), "{text}");
        assert!(text.contains("Rust toolchain `stable`"), "{text}");
        assert!(
            text.contains("rustfmt config [detected from rustfmt.toml]"),
            "{text}"
        );
        assert!(text.contains("has Cargo features"), "{text}");
    }

    #[test]
    fn empty_project_says_nothing_was_detected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let text = render(&build_playbook(tmp.path()));
        assert!(text.contains("NOTHING DETECTED"), "{text}");
        assert!(text.contains("COVERAGE: checked"), "{text}");
    }
}

#[cfg(test)]
mod dogfood {
    /// TrackerTop's own repository: its CI runs clippy with -D warnings in both feature sets.
    #[test]
    fn own_repo_playbook_reads_real_ci() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = super::render(&super::build_playbook(root));
        println!("{text}");
        assert!(text.contains("cargo clippy --all-targets"), "{text}");
        assert!(text.contains("no-default-features"), "{text}");
        assert!(text.contains(".github/workflows/ci.yml job"), "{text}");
        assert!(
            text.contains("Release/deploy workflows (not checks, not listed): release.yml"),
            "{text}"
        );
        assert!(
            text.contains(
                "format: `cargo fmt --check` [detected from Cargo.toml, confirmed by CI]"
            ),
            "{text}"
        );
    }
}
