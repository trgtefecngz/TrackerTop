//! Deterministic review rules over a `ChangeSet` and the head/base graphs.
//!
//! Every finding has a stable id (`rule:path:symbol`), a severity, a `file:line` location
//! and the evidence it rests on, so an agent can verify it in one read. A rule that can
//! only guess says so in its message and severity; nothing is reported as certain when
//! it is not.

use super::{Change, ChangeSet, ChangedSymbol, Span};
use crate::graph::CodeGraph;
use crate::model::{EdgeKind, Symbol, SymbolKind};
use petgraph::Direction;
use petgraph::visit::EdgeRef;
use serde::Serialize;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::Path;

/// Who depends on a symbol through code: calls, tests, type uses and trait impls. Doc
/// mentions (`References` from Markdown), file imports and co-change history are left out:
/// they are not callers, and counting them inflates a blast radius.
struct Dependents {
    direct_dependents: Vec<Symbol>,
    transitive_dependents: Vec<Symbol>,
    related_tests: Vec<Symbol>,
    affected_files: BTreeSet<String>,
}

fn code_dependents(cg: &CodeGraph, id: &str, depth: usize) -> Option<Dependents> {
    let root = cg.symbols.get(id)?;
    let &start = cg.id_to_idx.get(id)?;
    let mut d = Dependents {
        direct_dependents: Vec::new(),
        transitive_dependents: Vec::new(),
        related_tests: Vec::new(),
        affected_files: BTreeSet::from([root.file_path.clone()]),
    };
    let mut seen = HashSet::from([start]);
    let mut queue = VecDeque::from([(start, 0usize)]);
    while let Some((node, dist)) = queue.pop_front() {
        if dist >= depth {
            continue;
        }
        for e in cg.graph.edges_directed(node, Direction::Incoming) {
            let kind = *e.weight();
            if !matches!(
                kind,
                EdgeKind::Calls | EdgeKind::Tests | EdgeKind::UsesType | EdgeKind::Implements
            ) || !seen.insert(e.source())
            {
                continue;
            }
            let Some(sym) = cg.symbols.get(&cg.graph[e.source()]) else {
                continue;
            };
            if matches!(
                sym.kind,
                SymbolKind::File | SymbolKind::Section | SymbolKind::Dependency
            ) {
                continue;
            }
            d.affected_files.insert(sym.file_path.clone());
            if sym.kind == SymbolKind::Test || kind == EdgeKind::Tests {
                d.related_tests.push(sym.clone());
            } else if dist == 0 {
                d.direct_dependents.push(sym.clone());
            } else {
                d.transitive_dependents.push(sym.clone());
            }
            queue.push_back((e.source(), dist + 1));
        }
    }
    Some(d)
}

/// Dependents (within `IMPACT_DEPTH` hops) from which a change is worth calling out.
const IMPACT_LOW: usize = 8;
const IMPACT_MEDIUM: usize = 25;
const IMPACT_DEPTH: usize = 3;
/// A test reaching a symbol within this many call hops counts as covering it.
const TEST_DEPTH: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Stable across runs: `rule:path:qualified_name`.
    pub id: String,
    pub rule: &'static str,
    pub severity: Severity,
    pub symbol: String,
    /// `path:Lstart-end` of the symbol (head side, or base side if it was removed).
    pub location: String,
    pub message: String,
    /// `file:line` facts the finding rests on.
    pub evidence: Vec<String>,
}

fn loc(s: &Span) -> String {
    format!("{}:L{}-{}", s.path, s.start_line, s.end_line)
}

/// Functions and methods: the symbols whose callers and tests the graph tracks.
fn is_callable(kind: &str) -> bool {
    [SymbolKind::Function, SymbolKind::Method]
        .iter()
        .any(|k| k.as_str() == kind)
}

fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.starts_with("tests/")
        || p.contains("/tests/")
        || p.contains("/test/")
        || p.starts_with("test/")
        || p.contains("_test.")
        || p.contains(".test.")
        || p.contains(".spec.")
        || p.rsplit('/').next().is_some_and(|f| f.starts_with("test_"))
}

/// The graph symbol behind one side of a changed symbol.
fn symbol_at<'a>(cg: &'a CodeGraph, span: &Span, qname: &str) -> Option<&'a Symbol> {
    cg.symbols_by_file.get(&span.path)?.iter().find_map(|id| {
        cg.symbols
            .get(id)
            .filter(|s| s.qualified_name == qname && s.start_line == span.start_line)
    })
}

/// Lines `[start, end]` (1-based) of a file under `root`.
fn read_lines(root: &Path, path: &str, start: usize, end: usize) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(root.join(path)).ok()?;
    Some(
        text.lines()
            .enumerate()
            .filter(|(i, _)| *i + 1 >= start && *i < end)
            .map(|(_, l)| l.to_string())
            .collect(),
    )
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Whether `line` calls `needle`: `needle` not preceded by an identifier character and
/// followed (after optional spaces or a turbofish) by `(` or `!(`.
fn calls(line: &str, needle: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with('#') || t.starts_with('*') || t.starts_with("/*") {
        return false;
    }
    let mut from = 0;
    while let Some(pos) = line[from..].find(needle) {
        let at = from + pos;
        // A `.method` needle carries its own boundary: the receiver precedes the dot
        let before_ok =
            needle.starts_with('.') || line[..at].chars().next_back().is_none_or(|c| !is_ident(c));
        let rest = line[at + needle.len()..].trim_start();
        let rest = rest.strip_prefix('!').unwrap_or(rest);
        let rest = if rest.starts_with("::<") {
            rest.find('(').map_or("", |i| &rest[i..])
        } else {
            rest
        };
        if before_ok && rest.starts_with('(') {
            return true;
        }
        from = at + needle.len();
    }
    false
}

/// `broken-ref`: a symbol removed by the change that a surviving caller still calls.
///
/// Callers come from the base graph (who called it before). Each one that still exists at
/// head is re-read from the working tree, and the finding is only raised if the call text
/// is still there, with the matching line as evidence. A qualified or free-function call
/// is high severity; a bare `.method(` match is medium, since the receiver's type is not
/// known.
fn broken_refs(
    cs: &ChangeSet,
    head: &CodeGraph,
    base: &CodeGraph,
    head_root: &Path,
    out: &mut Vec<Finding>,
) {
    let head_qnames: BTreeSet<&str> = head
        .symbols
        .values()
        .map(|s| s.qualified_name.as_str())
        .collect();
    for cs_sym in cs.symbols.iter().filter(|s| s.change == Change::Removed) {
        // Moved to another file, not removed
        if head_qnames.contains(cs_sym.qualified_name.as_str()) {
            continue;
        }
        let Some(span) = &cs_sym.base else { continue };
        let Some(removed) = symbol_at(base, span, &cs_sym.qualified_name) else {
            continue;
        };
        let Some(report) = code_dependents(base, &removed.id, 1) else {
            continue;
        };
        let name = cs_sym
            .qualified_name
            .rsplit("::")
            .next()
            .unwrap_or(&cs_sym.qualified_name);
        let qualifier = cs_sym
            .qualified_name
            .rsplit_once("::")
            .map(|(q, _)| q.rsplit("::").next().unwrap_or(q));
        let mut evidence = Vec::new();
        let mut certain = false;
        let callers = report.direct_dependents.iter().chain(&report.related_tests);
        for caller in callers {
            // The caller must still exist at head (same name and kind, same or any file)
            let Some(now) = head
                .symbols
                .values()
                .filter(|s| s.qualified_name == caller.qualified_name && s.kind == caller.kind)
                .min_by_key(|s| (s.file_path != caller.file_path, s.start_line))
            else {
                continue;
            };
            let Some(lines) = read_lines(head_root, &now.file_path, now.start_line, now.end_line)
            else {
                continue;
            };
            for (i, line) in lines.iter().enumerate() {
                let qualified_hit = qualifier.is_some_and(|q| calls(line, &format!("{q}::{name}")));
                let free_hit = qualifier.is_none() && calls(line, name);
                let method_hit = qualifier.is_some() && calls(line, &format!(".{name}"));
                if qualified_hit || free_hit || method_hit {
                    certain |= qualified_hit || free_hit;
                    evidence.push(format!(
                        "{}:{} in {}: `{}`",
                        now.file_path,
                        now.start_line + i,
                        now.qualified_name,
                        line.trim()
                    ));
                    break;
                }
            }
        }
        if evidence.is_empty() {
            continue;
        }
        out.push(Finding {
            id: format!("broken-ref:{}:{}", span.path, cs_sym.qualified_name),
            rule: "broken-ref",
            severity: if certain {
                Severity::High
            } else {
                Severity::Medium
            },
            symbol: cs_sym.qualified_name.clone(),
            location: format!("was {}", loc(span)),
            message: format!(
                "`{}` was removed but {} caller(s) still call it{}",
                cs_sym.qualified_name,
                evidence.len(),
                if certain {
                    ""
                } else {
                    " (matched as `.method(` only; the receiver's type is not checked)"
                }
            ),
            evidence,
        });
    }
}

/// The head symbol behind an added or modified callable, unless it is test code.
fn changed_callable<'a>(
    head: &'a CodeGraph,
    s: &'a ChangedSymbol,
) -> Option<(&'a Symbol, &'a Span)> {
    if s.change == Change::Removed || !is_callable(&s.kind) {
        return None;
    }
    let span = s.head.as_ref()?;
    if is_test_path(&span.path) {
        return None;
    }
    let sym = symbol_at(head, span, &s.qualified_name)?;
    (sym.kind != SymbolKind::Test).then_some((sym, span))
}

/// `untested`: an added or modified function that no test reaches within `TEST_DEPTH`
/// call hops. Call edges are heuristic for some languages, so this is low severity.
fn untested(cs: &ChangeSet, head: &CodeGraph, out: &mut Vec<Finding>) {
    for s in &cs.symbols {
        let Some((sym, span)) = changed_callable(head, s) else {
            continue;
        };
        if sym.name == "main" {
            continue;
        }
        let Some(report) = code_dependents(head, &sym.id, TEST_DEPTH) else {
            continue;
        };
        if !report.related_tests.is_empty() {
            continue;
        }
        out.push(Finding {
            id: format!("untested:{}:{}", span.path, s.qualified_name),
            rule: "untested",
            severity: Severity::Low,
            symbol: s.qualified_name.clone(),
            location: loc(span),
            message: format!(
                "{} `{}` is not reached by any test within {TEST_DEPTH} call hops \
                 (call edges are partly heuristic: check before writing a test)",
                s.change.as_str(),
                s.qualified_name
            ),
            evidence: vec![format!(
                "{} direct and {} transitive callers, none of them a test",
                report.direct_dependents.len(),
                report.transitive_dependents.len()
            )],
        });
    }
}

/// `impact`: a modified function with many dependents; review it and run its tests.
fn impact(cs: &ChangeSet, head: &CodeGraph, out: &mut Vec<Finding>) {
    for s in cs.symbols.iter().filter(|s| s.change == Change::Modified) {
        let Some((sym, span)) = changed_callable(head, s) else {
            continue;
        };
        let Some(report) = code_dependents(head, &sym.id, IMPACT_DEPTH) else {
            continue;
        };
        let dependents = report.direct_dependents.len() + report.transitive_dependents.len();
        if dependents < IMPACT_LOW {
            continue;
        }
        let mut evidence: Vec<String> = report
            .direct_dependents
            .iter()
            .take(5)
            .map(|d| format!("{}:{} {}", d.file_path, d.start_line, d.qualified_name))
            .collect();
        if report.direct_dependents.len() > 5 {
            evidence.push(format!(
                "... {} more direct callers",
                report.direct_dependents.len() - 5
            ));
        }
        evidence.push(format!(
            "tests that reach it: {}",
            if report.related_tests.is_empty() {
                "none found".to_string()
            } else {
                report
                    .related_tests
                    .iter()
                    .take(5)
                    .map(|t| t.qualified_name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        out.push(Finding {
            id: format!("impact:{}:{}", span.path, s.qualified_name),
            rule: "impact",
            severity: if dependents >= IMPACT_MEDIUM {
                Severity::Medium
            } else {
                Severity::Low
            },
            symbol: s.qualified_name.clone(),
            location: loc(span),
            message: format!(
                "modified `{}` has {} dependents within {IMPACT_DEPTH} hops ({} direct) in {} files",
                s.qualified_name,
                dependents,
                report.direct_dependents.len(),
                report.affected_files.len()
            ),
            evidence,
        });
    }
}

/// Runs every rule. Findings are sorted by severity, then id.
pub fn run_rules(
    cs: &ChangeSet,
    head: &CodeGraph,
    base: &CodeGraph,
    head_root: &Path,
) -> Vec<Finding> {
    let mut out = Vec::new();
    broken_refs(cs, head, base, head_root, &mut out);
    impact(cs, head, &mut out);
    untested(cs, head, &mut out);
    out.sort_by(|a, b| a.severity.cmp(&b.severity).then_with(|| a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

#[cfg(test)]
mod tests {
    use super::calls;

    #[test]
    fn call_text_matching_respects_identifier_boundaries() {
        assert!(calls("    let x = helper(1);", "helper"));
        assert!(calls("helper (1)", "helper"));
        assert!(calls("parse::<u8>(s)", "parse"));
        assert!(calls("debug_log!(x)", "debug_log"));
        assert!(calls("Store::open(p)", "Store::open"));
        assert!(calls("s.flush()", ".flush"));
        assert!(!calls("my_helper(1)", "helper"));
        assert!(!calls("helper_two(1)", "helper"));
        assert!(!calls("let helper = 3;", "helper"));
        assert!(!calls("// mentions helper only", "helper"));
        assert!(!calls("    // old: helper(1)", "helper"));
    }
}
