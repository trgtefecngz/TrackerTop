//! `run_checks`: run the project's own test / lint / build / format commands and turn their
//! output into a compact summary for an AI agent.
//!
//! The summary is built so that a failure is never hidden:
//! - every failing test name and every error diagnostic recognized in the output is listed
//!   (with file:line where the tool printed one);
//! - a non-zero exit code with nothing recognized is reported as an UNRECOGNIZED FAILURE
//!   together with the tail of the raw output;
//! - a timeout says the results are partial.
//!
//! Commands are detected from project files (Cargo.toml, package.json, go.mod, pyproject…)
//! and executed directly, without a shell; extra arguments are passed as separate argv items.

use crate::tokens::count_tokens;
use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    Test,
    Lint,
    Build,
    Format,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckKind::Test => "test",
            CheckKind::Lint => "lint",
            CheckKind::Build => "build",
            CheckKind::Format => "format",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "test" | "tests" => Some(CheckKind::Test),
            "lint" | "clippy" => Some(CheckKind::Lint),
            "build" | "compile" => Some(CheckKind::Build),
            "format" | "fmt" => Some(CheckKind::Format),
            _ => None,
        }
    }
}

/// One runnable project command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckCommand {
    pub kind: CheckKind,
    pub program: String,
    /// Arguments before any extra arguments the agent passes.
    pub args: Vec<String>,
    /// Inserted before extra arguments when there are any (`npm test -- <extra>`).
    pub extra_separator: Option<String>,
    /// Arguments after the extra arguments (`cargo clippy <extra> -- -D warnings`).
    pub suffix: Vec<String>,
    /// File the command was derived from, e.g. `Cargo.toml` or `package.json scripts.test`.
    pub source: String,
    /// true: the project declares it (a script, a config section); false: the ecosystem
    /// default for a detected project type.
    pub detected: bool,
}

impl CheckCommand {
    fn new(kind: CheckKind, program: &str, args: &[&str], source: &str, detected: bool) -> Self {
        Self {
            kind,
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            extra_separator: None,
            suffix: Vec::new(),
            source: source.to_string(),
            detected,
        }
    }

    fn with_suffix(mut self, suffix: &[&str]) -> Self {
        self.suffix = suffix.iter().map(|s| s.to_string()).collect();
        self
    }

    fn with_separator(mut self, sep: &str) -> Self {
        self.extra_separator = Some(sep.to_string());
        self
    }

    /// Full argv after the program, with `extra` inserted at the right place.
    pub fn argv(&self, extra: &[String]) -> Vec<String> {
        let mut v = self.args.clone();
        if !extra.is_empty() {
            if let Some(sep) = &self.extra_separator {
                v.push(sep.clone());
            }
            v.extend(extra.iter().cloned());
        }
        v.extend(self.suffix.iter().cloned());
        v
    }

    pub fn display(&self, extra: &[String]) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.argv(extra));
        parts.join(" ")
    }
}

/// Commands the project declares or that are the standard for its detected toolchains.
/// Order: Rust, JavaScript/TypeScript, Go, Python (a repo can have several).
pub fn detect_commands(root: &Path) -> Vec<CheckCommand> {
    let mut out = Vec::new();
    if root.join("Cargo.toml").is_file() {
        let src = "Cargo.toml";
        out.push(CheckCommand::new(
            CheckKind::Test,
            "cargo",
            &["test"],
            src,
            true,
        ));
        out.push(
            CheckCommand::new(
                CheckKind::Lint,
                "cargo",
                &["clippy", "--all-targets"],
                src,
                false,
            )
            .with_suffix(&["--", "-D", "warnings"]),
        );
        out.push(CheckCommand::new(
            CheckKind::Build,
            "cargo",
            &["build"],
            src,
            true,
        ));
        out.push(CheckCommand::new(
            CheckKind::Format,
            "cargo",
            &["fmt", "--check"],
            src,
            false,
        ));
    }
    if let Ok(text) = std::fs::read_to_string(root.join("package.json"))
        && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let pm = if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
            "bun"
        } else {
            "npm"
        };
        let scripts = pkg.get("scripts").and_then(|s| s.as_object());
        let has = |name: &str| {
            scripts
                .and_then(|s| s.get(name))
                .and_then(|v| v.as_str())
                .is_some_and(|cmd| !cmd.contains("no test specified"))
        };
        for (kind, names) in [
            (CheckKind::Test, &["test"][..]),
            (CheckKind::Lint, &["lint"][..]),
            (CheckKind::Build, &["build", "typecheck"][..]),
            (
                CheckKind::Format,
                &["format:check", "fmt:check", "prettier:check"][..],
            ),
        ] {
            if let Some(script) = names.iter().find(|n| has(n)) {
                let source = format!("package.json scripts.{script}");
                // npm/pnpm/yarn/bun all accept `run <script> -- <args>`
                out.push(
                    CheckCommand::new(kind, pm, &["run", script], &source, true)
                        .with_separator("--"),
                );
            }
        }
    }
    if root.join("go.mod").is_file() {
        let src = "go.mod";
        out.push(CheckCommand::new(
            CheckKind::Test,
            "go",
            &["test", "./..."],
            src,
            true,
        ));
        out.push(CheckCommand::new(
            CheckKind::Lint,
            "go",
            &["vet", "./..."],
            src,
            false,
        ));
        out.push(CheckCommand::new(
            CheckKind::Build,
            "go",
            &["build", "./..."],
            src,
            true,
        ));
    }
    let pyproject = std::fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default();
    let pytest_configured = pyproject.contains("[tool.pytest")
        || root.join("pytest.ini").is_file()
        || root.join("conftest.py").is_file()
        || std::fs::read_to_string(root.join("setup.cfg"))
            .is_ok_and(|s| s.contains("[tool:pytest]"));
    if pytest_configured {
        let src = if pyproject.contains("[tool.pytest") {
            "pyproject.toml [tool.pytest]"
        } else {
            "pytest config"
        };
        out.push(CheckCommand::new(
            CheckKind::Test,
            "python",
            &["-m", "pytest", "-q"],
            src,
            true,
        ));
    }
    if pyproject.contains("[tool.ruff") || root.join("ruff.toml").is_file() {
        out.push(CheckCommand::new(
            CheckKind::Lint,
            "ruff",
            &["check", "."],
            "ruff config",
            true,
        ));
    }
    out
}

/// Raw result of one command run.
#[derive(Debug, Clone)]
pub struct CheckRun {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// stdout followed by stderr, ANSI color codes removed.
    pub output: String,
}

impl CheckRun {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

fn resolve_program(program: &str) -> String {
    if cfg!(windows) && matches!(program, "npm" | "npx" | "pnpm" | "yarn") {
        format!("{program}.cmd")
    } else {
        program.to_string()
    }
}

/// Runs `cmd` in `root` without a shell, killing it after `timeout`.
pub fn run_command(
    root: &Path,
    cmd: &CheckCommand,
    extra: &[String],
    timeout: Duration,
) -> Result<CheckRun> {
    let display = cmd.display(extra);
    let started = Instant::now();
    let mut child = Command::new(resolve_program(&cmd.program))
        .args(cmd.argv(extra))
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CARGO_TERM_COLOR", "never")
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .env("CI", "1")
        .spawn()
        .with_context(|| {
            format!(
                "could not start `{display}` (is `{}` installed?)",
                cmd.program
            )
        })?;

    let mut stdout = child.stdout.take().context("no stdout pipe")?;
    let mut stderr = child.stderr.take().context("no stderr pipe")?;
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    let mut output = String::from_utf8_lossy(&out).into_owned();
    if !err.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&String::from_utf8_lossy(&err));
    }
    Ok(CheckRun {
        command: display,
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
        output: strip_ansi(&output),
    })
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// One failing test with the few lines that explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub name: String,
    pub details: Vec<String>,
}

/// What could be recognized in a command's output.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CheckSummary {
    pub passed: Option<usize>,
    pub failed: Option<usize>,
    pub ignored: Option<usize>,
    pub failures: Vec<Failure>,
    /// Error diagnostics (compiler, linter, formatter), with location when printed.
    pub errors: Vec<String>,
    pub warnings: usize,
}

impl CheckSummary {
    fn add_counts(&mut self, passed: usize, failed: usize, ignored: usize) {
        *self.passed.get_or_insert(0) += passed;
        *self.failed.get_or_insert(0) += failed;
        *self.ignored.get_or_insert(0) += ignored;
    }

    fn add_failure(&mut self, name: &str) {
        let name = name.trim();
        if !name.is_empty() && !self.failures.iter().any(|f| f.name == name) {
            self.failures.push(Failure {
                name: name.to_string(),
                details: Vec::new(),
            });
        }
    }

    fn add_detail(&mut self, name: &str, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if let Some(f) = self.failures.iter_mut().find(|f| f.name == name)
            && f.details.len() < 6
            && !f.details.iter().any(|d| d == line)
        {
            f.details.push(line.to_string());
        }
    }

    /// Whether anything that indicates a problem was recognized.
    pub fn found_problems(&self) -> bool {
        !self.failures.is_empty() || !self.errors.is_empty() || self.failed.is_some_and(|n| n > 0)
    }
}

/// Number right before `label` in `s` (`"12 passed"` → 12 for label `passed`).
fn count_before(s: &str, label: &str) -> Option<usize> {
    let idx = s.find(label)?;
    let head = s[..idx].trim_end();
    let digits: String = head
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

fn is_detail_line(line: &str) -> bool {
    let l = line.trim_start();
    let lower = l.to_ascii_lowercase();
    lower.contains("panicked at")
        || lower.starts_with("assertion")
        || lower.starts_with("left:")
        || lower.starts_with("right:")
        || lower.starts_with("expected")
        || lower.starts_with("received")
        || lower.starts_with("error")
        || lower.starts_with("e ")
        || lower.contains("assertionerror")
        || lower.contains("thread '")
        || l.starts_with("at ") && l.contains(':')
        || l.contains(".rs:")
        || l.contains(".py:")
        || l.contains(".go:")
        || l.contains(".ts:")
        || l.contains(".js:")
}

/// Recognizes cargo/rustc, Jest, Vitest, pytest and `go test` output.
pub fn summarize(output: &str) -> CheckSummary {
    let mut s = CheckSummary::default();
    let lines: Vec<&str> = output.lines().collect();
    let mut current: Option<String> = None; // failure whose detail block we are in

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim_end();
        let t = line.trim();

        // ---- cargo test ----
        if let Some(rest) = t.strip_prefix("test result: ") {
            s.add_counts(
                count_before(rest, " passed").unwrap_or(0),
                count_before(rest, " failed").unwrap_or(0),
                count_before(rest, " ignored").unwrap_or(0),
            );
            current = None;
            continue;
        }
        if let Some(rest) = t.strip_prefix("test ")
            && let Some(name) = rest.strip_suffix(" ... FAILED")
        {
            s.add_failure(name);
            continue;
        }
        if let Some(rest) = t.strip_prefix("---- ")
            && let Some(name) = rest
                .strip_suffix(" stdout ----")
                .or_else(|| rest.strip_suffix(" stderr ----"))
        {
            s.add_failure(name);
            current = Some(name.trim().to_string());
            continue;
        }
        if t == "failures:" || t.starts_with("test result") {
            current = None;
        }

        // ---- rustc / clippy diagnostics ----
        if (t.starts_with("error[") || t.starts_with("error:"))
            && !t.starts_with("error: test failed")
            && !t.starts_with("error: could not compile")
            && !t.starts_with("error: aborting due to")
            && !t.starts_with("error: process didn't exit successfully")
            && !t.starts_with("error: Recipe")
        {
            let loc = lines
                .iter()
                .skip(i + 1)
                .take(3)
                .find_map(|l| l.trim_start().strip_prefix("--> "))
                .map(|l| format!(" ({})", l.trim()))
                .unwrap_or_default();
            s.errors.push(format!("{t}{loc}"));
            continue;
        }
        if t.starts_with("warning:")
            && !t.contains(" generated ")
            && !t.contains("warnings emitted")
        {
            s.warnings += 1;
        }
        // cargo fmt --check: `Diff in /path/file.rs at line 12:`
        if let Some(rest) = t.strip_prefix("Diff in ") {
            s.errors
                .push(format!("not formatted: {}", rest.trim_end_matches(':')));
            continue;
        }

        // ---- Jest ----
        if let Some(rest) = t.strip_prefix("Tests:") {
            s.add_counts(
                count_before(rest, " passed").unwrap_or(0),
                count_before(rest, " failed").unwrap_or(0),
                count_before(rest, " skipped").unwrap_or(0)
                    + count_before(rest, " todo").unwrap_or(0),
            );
            continue;
        }
        if t.starts_with("● Test suite failed to run") {
            s.errors.push("Test suite failed to run".to_string());
            continue;
        }
        if let Some(name) = t.strip_prefix("● ") {
            s.add_failure(name);
            current = Some(name.trim().to_string());
            continue;
        }

        // ---- Vitest ----
        if let Some(rest) = t.strip_prefix("Tests ")
            && (rest.contains(" passed") || rest.contains(" failed"))
            && rest.contains('(')
        {
            s.add_counts(
                count_before(rest, " passed").unwrap_or(0),
                count_before(rest, " failed").unwrap_or(0),
                count_before(rest, " skipped").unwrap_or(0),
            );
            continue;
        }
        if let Some(rest) = t.strip_prefix("FAIL ")
            && rest.contains(" > ")
        {
            let name = rest.trim();
            s.add_failure(name);
            current = Some(name.to_string());
            continue;
        }

        // ---- pytest ----
        if let Some(rest) = t.strip_prefix("FAILED ") {
            let (name, msg) = rest.split_once(" - ").unwrap_or((rest, ""));
            s.add_failure(name);
            if !msg.is_empty() {
                s.add_detail(name.trim(), msg);
            }
            continue;
        }
        if t.starts_with('=')
            && t.ends_with('=')
            && (t.contains(" passed") || t.contains(" failed"))
            && t.contains(" in ")
        {
            s.add_counts(
                count_before(t, " passed").unwrap_or(0),
                count_before(t, " failed").unwrap_or(0),
                count_before(t, " skipped").unwrap_or(0),
            );
            continue;
        }
        if let Some(rest) = t.strip_prefix("ERROR ") {
            s.errors.push(format!("ERROR {rest}"));
            continue;
        }

        // ---- go test ----
        if let Some(rest) = t.strip_prefix("--- FAIL: ") {
            let name = rest.split(" (").next().unwrap_or(rest);
            s.add_failure(name);
            current = Some(name.trim().to_string());
            continue;
        }
        if t.starts_with("--- PASS: ") {
            *s.passed.get_or_insert(0) += 1;
            continue;
        }

        if let Some(name) = &current
            && is_detail_line(t)
        {
            let name = name.clone();
            s.add_detail(&name, t);
        }
    }

    // Never report fewer failures than distinct failing names we saw.
    if s.failures.len() > s.failed.unwrap_or(0) {
        s.failed = Some(s.failures.len());
    }
    s
}

/// Compact report for one run. Failing names come before details so that the most
/// important part survives a tight token budget.
pub fn render(cmd: &CheckCommand, run: &CheckRun, summary: &CheckSummary) -> String {
    let mut out = format!(
        "# RUN_CHECKS {}: `{}` ({} from {})\n",
        cmd.kind.as_str(),
        run.command,
        if cmd.detected { "detected" } else { "guessed" },
        cmd.source
    );
    let status = if run.timed_out {
        "TIMEOUT".to_string()
    } else if run.succeeded() && !summary.found_problems() {
        "PASSED".to_string()
    } else {
        match run.exit_code {
            Some(code) => format!("FAILED (exit {code})"),
            None => "FAILED (killed by signal)".to_string(),
        }
    };
    let mut counts = Vec::new();
    if let Some(p) = summary.passed {
        counts.push(format!("{p} passed"));
    }
    if let Some(f) = summary.failed {
        counts.push(format!("{f} failed"));
    }
    if let Some(ig) = summary.ignored.filter(|n| *n > 0) {
        counts.push(format!("{ig} ignored/skipped"));
    }
    if !summary.errors.is_empty() {
        counts.push(format!("{} errors", summary.errors.len()));
    }
    if summary.warnings > 0 {
        counts.push(format!("{} warnings", summary.warnings));
    }
    out.push_str(&format!(
        "RESULT: {status} in {:.1}s{}\n",
        run.duration_ms as f64 / 1000.0,
        if counts.is_empty() {
            String::new()
        } else {
            format!(" — {}", counts.join(", "))
        }
    ));
    if run.timed_out {
        out.push_str("TIMEOUT: the process was killed; everything below is PARTIAL.\n");
    }
    if run.succeeded() && summary.found_problems() {
        out.push_str(
            "NOTE: exit code 0 but failures/errors were recognized in the output; check them.\n",
        );
    }

    if !summary.failures.is_empty() {
        out.push_str(&format!(
            "\n## Failing tests ({})\n",
            summary.failures.len()
        ));
        for f in &summary.failures {
            out.push_str(&format!("- {}\n", f.name));
        }
    }
    if !summary.errors.is_empty() {
        out.push_str(&format!("\n## Errors ({})\n", summary.errors.len()));
        for e in &summary.errors {
            out.push_str(&format!("- {e}\n"));
        }
    }
    if summary.failures.iter().any(|f| !f.details.is_empty()) {
        out.push_str("\n## Failure details\n");
        for f in summary.failures.iter().filter(|f| !f.details.is_empty()) {
            out.push_str(&format!("### {}\n", f.name));
            for d in &f.details {
                out.push_str(&format!("  {d}\n"));
            }
        }
    }

    if run.timed_out || (!run.succeeded() && !summary.found_problems()) {
        if run.timed_out {
            out.push_str("\nLast lines of output:\n");
        } else {
            out.push_str(
                "\nUNRECOGNIZED FAILURE: the command failed but no failing test or error line \
                 was recognized. Last lines of output:\n",
            );
        }
        let tail: Vec<&str> = run.output.lines().rev().take(40).collect();
        out.push_str("```\n");
        for l in tail.into_iter().rev() {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str("```\n");
    }
    out.push_str(&format!(
        "COVERAGE: parsed {} output lines ({} tok raw) with the cargo/jest/vitest/pytest/go \
         recognizers.\n",
        run.output.lines().count(),
        count_tokens(&run.output)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(output: &str, exit: i32) -> CheckRun {
        CheckRun {
            command: "x".into(),
            exit_code: Some(exit),
            timed_out: false,
            duration_ms: 10,
            output: output.into(),
        }
    }

    fn cmd() -> CheckCommand {
        CheckCommand::new(CheckKind::Test, "cargo", &["test"], "Cargo.toml", true)
    }

    const CARGO_FAIL: &str = "\
running 3 tests
test a::ok_one ... ok
test a::bad_one ... FAILED
test a::bad_two ... FAILED

failures:

---- a::bad_one stdout ----

thread 'a::bad_one' panicked at src/a.rs:10:5:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

---- a::bad_two stdout ----
thread 'a::bad_two' panicked at src/a.rs:20:9:
boom

failures:
    a::bad_one
    a::bad_two

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

running 4 tests
test result: ok. 4 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
error: test failed, to rerun pass `--lib`
";

    #[test]
    fn cargo_test_failures_counts_and_details() {
        let s = summarize(CARGO_FAIL);
        assert_eq!((s.passed, s.failed, s.ignored), (Some(5), Some(2), Some(1)));
        let names: Vec<&str> = s.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["a::bad_one", "a::bad_two"]);
        let d = &s.failures[0].details;
        assert!(d.iter().any(|l| l.contains("src/a.rs:10:5")), "{d:?}");
        assert!(d.iter().any(|l| l.starts_with("left: 1")), "{d:?}");
        assert!(
            s.errors.is_empty(),
            "summary lines are not diagnostics: {:?}",
            s.errors
        );
        let text = render(&cmd(), &run(CARGO_FAIL, 101), &s);
        assert!(text.contains("RESULT: FAILED (exit 101)"), "{text}");
        assert!(
            text.contains("5 passed, 2 failed, 1 ignored/skipped"),
            "{text}"
        );
        assert!(text.contains("- a::bad_two"), "{text}");
        assert!(!text.contains("UNRECOGNIZED"), "{text}");
    }

    #[test]
    fn rustc_errors_keep_their_location() {
        let out = "   Compiling x v0.1.0\n\
                   error[E0425]: cannot find value `y` in this scope\n  \
                   --> src/lib.rs:3:5\n   |\n\
                   warning: unused variable: `z`\n  --> src/lib.rs:9:9\n\
                   error: could not compile `x` (lib) due to 1 previous error\n";
        let s = summarize(out);
        assert_eq!(
            s.errors,
            vec!["error[E0425]: cannot find value `y` in this scope (src/lib.rs:3:5)"]
        );
        assert_eq!(s.warnings, 1);
    }

    #[test]
    fn jest_vitest_pytest_and_go_are_recognized() {
        let jest = "FAIL src/a.test.ts\n  ● math › adds\n\n    expect(received).toBe(expected)\n    Expected: 3\n    Received: 4\n\n      at Object.<anonymous> (src/a.test.ts:5:17)\n\nTests:       1 failed, 7 passed, 8 total\n";
        let s = summarize(jest);
        assert_eq!((s.passed, s.failed), (Some(7), Some(1)));
        assert_eq!(s.failures[0].name, "math › adds");
        assert!(
            s.failures[0]
                .details
                .iter()
                .any(|d| d.contains("a.test.ts:5:17"))
        );

        let vitest = " FAIL  src/a.test.ts > math > adds\nAssertionError: expected 4 to be 3\n Tests  1 failed | 5 passed (6)\n";
        let s = summarize(vitest);
        assert_eq!((s.passed, s.failed), (Some(5), Some(1)));
        assert_eq!(s.failures[0].name, "src/a.test.ts > math > adds");

        let pytest = "FAILED tests/test_a.py::test_add - assert 4 == 3\n==== 1 failed, 9 passed in 0.12s ====\n";
        let s = summarize(pytest);
        assert_eq!((s.passed, s.failed), (Some(9), Some(1)));
        assert_eq!(s.failures[0].name, "tests/test_a.py::test_add");
        assert_eq!(s.failures[0].details, vec!["assert 4 == 3"]);

        let go = "--- FAIL: TestAdd (0.00s)\n    add_test.go:9: got 4 want 3\n--- PASS: TestSub (0.00s)\nFAIL\nFAIL\texample.com/m\t0.002s\n";
        let s = summarize(go);
        assert_eq!((s.passed, s.failed), (Some(1), Some(1)));
        assert_eq!(s.failures[0].name, "TestAdd");
        assert!(
            s.failures[0]
                .details
                .iter()
                .any(|d| d.contains("add_test.go:9"))
        );
    }

    #[test]
    fn unrecognized_failure_is_never_hidden() {
        let out = "something odd happened\nSegmentation fault\n";
        let s = summarize(out);
        assert!(!s.found_problems());
        let text = render(&cmd(), &run(out, 139), &s);
        assert!(text.contains("RESULT: FAILED (exit 139)"), "{text}");
        assert!(text.contains("UNRECOGNIZED FAILURE"), "{text}");
        assert!(text.contains("Segmentation fault"), "{text}");
    }

    #[test]
    fn exit_zero_with_failures_is_not_reported_as_passed() {
        let out = "test a::x ... FAILED\n";
        let s = summarize(out);
        let text = render(&cmd(), &run(out, 0), &s);
        assert!(text.contains("RESULT: FAILED (exit 0)"), "{text}");
        assert!(text.contains("NOTE: exit code 0"), "{text}");
    }

    #[test]
    fn passing_run_is_short() {
        let out = "running 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let s = summarize(out);
        let text = render(&cmd(), &run(out, 0), &s);
        assert!(text.contains("RESULT: PASSED"), "{text}");
        assert!(text.contains("2 passed, 0 failed"), "{text}");
        assert!(!text.contains("## "), "{text}");
    }

    #[test]
    fn fmt_check_diffs_are_errors() {
        let s = summarize("Diff in /repo/src/lib.rs at line 3:\n-a\n+b\n");
        assert_eq!(s.errors, vec!["not formatted: /repo/src/lib.rs at line 3"]);
    }

    #[test]
    fn extra_args_go_before_the_suffix_and_after_the_separator() {
        let clippy = CheckCommand::new(
            CheckKind::Lint,
            "cargo",
            &["clippy", "--all-targets"],
            "",
            false,
        )
        .with_suffix(&["--", "-D", "warnings"]);
        assert_eq!(
            clippy.display(&["-p".into(), "core".into()]),
            "cargo clippy --all-targets -p core -- -D warnings"
        );
        let npm = CheckCommand::new(CheckKind::Test, "npm", &["run", "test"], "", true)
            .with_separator("--");
        assert_eq!(npm.display(&[]), "npm run test");
        assert_eq!(
            npm.display(&["-t".into(), "adds".into()]),
            "npm run test -- -t adds"
        );
    }

    #[test]
    fn detection_reads_project_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"test":"vitest run","lint":"eslint .","build":"tsc -b"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.pytest.ini_options]\n[tool.ruff]\n",
        )
        .unwrap();
        let cmds = detect_commands(root);
        let shown: Vec<String> = cmds
            .iter()
            .map(|c| format!("{} {} {}", c.kind.as_str(), c.display(&[]), c.detected))
            .collect();
        assert_eq!(
            shown,
            vec![
                "test cargo test true",
                "lint cargo clippy --all-targets -- -D warnings false",
                "build cargo build true",
                "format cargo fmt --check false",
                "test pnpm run test true",
                "lint pnpm run lint true",
                "build pnpm run build true",
                "test python -m pytest -q true",
                "lint ruff check . true",
            ]
        );
    }

    #[test]
    fn npm_placeholder_test_script_is_not_a_test_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();
        assert!(detect_commands(tmp.path()).is_empty());
    }

    #[test]
    fn runs_a_real_command_and_reports_failures() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ok = CheckCommand::new(CheckKind::Build, "cargo", &["--version"], "test", true);
        let r = run_command(tmp.path(), &ok, &[], Duration::from_secs(60)).unwrap();
        assert!(r.succeeded(), "{r:?}");
        assert!(r.output.contains("cargo"), "{r:?}");

        let bad = CheckCommand::new(
            CheckKind::Build,
            "cargo",
            &["definitely-not-a-subcommand"],
            "test",
            true,
        );
        let r = run_command(tmp.path(), &bad, &[], Duration::from_secs(60)).unwrap();
        assert!(!r.succeeded());
        let text = render(&bad, &r, &summarize(&r.output));
        assert!(text.contains("FAILED (exit"), "{text}");
        assert!(text.contains("definitely-not-a-subcommand"), "{text}");

        let missing = CheckCommand::new(CheckKind::Build, "no-such-program-xyz", &[], "test", true);
        let err = run_command(tmp.path(), &missing, &[], Duration::from_secs(5)).unwrap_err();
        assert!(format!("{err:#}").contains("could not start"), "{err:#}");
    }
}
