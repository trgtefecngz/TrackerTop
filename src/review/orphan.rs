//! `orphan`: a function the change left without any caller.
//!
//! Only functions that *had* a caller the change removed or rewrote are considered, so
//! long-dead code is not blamed on this change. The graph's verdict ("no callers") is
//! never trusted alone: the name must also be absent, as a whole word, from every indexed
//! code file outside the function itself. Any mention (a call the graph cannot resolve,
//! a function passed as a value, a string in a registry) keeps the finding back.

use super::rules::{
    Finding, Severity, code_dependents, is_callable, is_ident, is_test_path, loc, symbol_at,
};
use super::{Change, ChangeSet, Span};
use crate::graph::CodeGraph;
use crate::model::{EdgeKind, Symbol, SymbolKind};
use petgraph::Direction;
use petgraph::visit::EdgeRef;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Whether `text` contains `name` as a whole identifier.
fn mentions(line: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = line[from..].find(name) {
        let at = from + pos;
        let end = at + name.len();
        let before = line[..at].chars().next_back().is_none_or(|c| !is_ident(c));
        let after = line[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

/// Every file of the project at head that git knows (tracked, or untracked and not
/// ignored), any type: a name in a YAML job list or an entry-point table keeps a function
/// alive as surely as a call. `None` when the list cannot be read or a file cannot be read
/// in full, since "named nowhere" could then not be confirmed.
fn project_texts(root: &Path) -> Option<HashMap<String, String>> {
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let list = crate::changes::git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
    .ok()?;
    let mut out = HashMap::new();
    for rel in list.split('\0').filter(|p| !p.is_empty()) {
        let path = root.join(rel);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue; // listed but deleted in the working tree
        };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_BYTES {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        out.insert(
            rel.replace('\\', "/"),
            String::from_utf8_lossy(&bytes).into_owned(),
        );
    }
    Some(out)
}

/// Attributes and decorators that do not register a function anywhere.
const PLAIN_ATTRS: [&str; 9] = [
    "#[allow",
    "#[inline",
    "#[must_use",
    "#[doc",
    "#[deny",
    "#[warn",
    "#[expect",
    "#[cold",
    "#[track_caller",
];

/// Whether the function carries an attribute or decorator that may register it with a
/// framework (`#[tauri::command]`, `@app.route`, `#[no_mangle]`): it can be reached with
/// no call anywhere in the code.
fn registered(text: Option<&String>, sym: &Symbol) -> bool {
    let Some(text) = text else { return true };
    let lines: Vec<&str> = text.lines().collect();
    let is_attr = |l: &str| l.starts_with("#[") || l.starts_with('@');
    // Attributes above the span, skipping doc comments, and any inside its first lines
    let above = lines[..sym.start_line.saturating_sub(1).min(lines.len())]
        .iter()
        .rev()
        .map(|l| l.trim())
        .take_while(|l| is_attr(l) || l.starts_with("///") || l.starts_with('#'));
    let inside = lines
        .iter()
        .skip(sym.start_line.saturating_sub(1))
        .map(|l| l.trim())
        .take_while(|l| !mentions(l, &sym.name));
    above
        .chain(inside)
        .filter(|l| is_attr(l))
        .any(|l| !PLAIN_ATTRS.iter().any(|p| l.starts_with(p)))
}

/// The same function at head: same file, qualified name and kind.
fn at_head<'a>(head: &'a CodeGraph, base_sym: &Symbol) -> Option<&'a Symbol> {
    head.symbols_by_file
        .get(&base_sym.file_path)?
        .iter()
        .filter_map(|id| head.symbols.get(id))
        .find(|s| s.qualified_name == base_sym.qualified_name && s.kind == base_sym.kind)
}

pub fn orphans(
    cs: &ChangeSet,
    head: &CodeGraph,
    base: &CodeGraph,
    head_root: &Path,
    out: &mut Vec<Finding>,
) {
    // callee base id -> the changed callers that used to call it
    let mut lost: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for s in &cs.symbols {
        if s.change == Change::Added {
            continue;
        }
        let Some(span) = &s.base else { continue };
        let Some(caller) = symbol_at(base, span, &s.qualified_name) else {
            continue;
        };
        let Some(&idx) = base.id_to_idx.get(&caller.id) else {
            continue;
        };
        for e in base.graph.edges_directed(idx, Direction::Outgoing) {
            if *e.weight() != EdgeKind::Calls {
                continue;
            }
            let callee = &base.graph[e.target()];
            if *callee != caller.id {
                lost.entry(callee.clone()).or_default().push(format!(
                    "{}:{} `{}` ({})",
                    span.path,
                    span.start_line,
                    s.qualified_name,
                    s.change.as_str()
                ));
            }
        }
    }

    // The project's files at head, read once and only if there is a candidate.
    let mut texts: Option<HashMap<String, String>> = None;
    for (callee_id, callers) in lost {
        let Some(c) = base.symbols.get(&callee_id) else {
            continue;
        };
        if !is_callable(c.kind.as_str())
            || c.kind == SymbolKind::Test
            || is_test_path(&c.file_path)
            || c.name == "main"
            || c.name.len() < 3
        {
            continue;
        }
        let Some(now) = at_head(head, c) else {
            continue; // removed or moved: other rules cover it
        };
        let Some(deps) = code_dependents(head, &now.id, 1) else {
            continue;
        };
        if !deps.direct_dependents.is_empty() || !deps.related_tests.is_empty() {
            continue;
        }
        if texts.is_none() {
            match project_texts(head_root) {
                Some(t) => texts = Some(t),
                // Without the file list nothing can be confirmed: report nothing
                None => return,
            }
        }
        let Some(texts) = texts.as_ref() else { return };
        if registered(texts.get(now.file_path.as_str()), now) {
            continue;
        }
        let still_named = texts.iter().any(|(path, text)| {
            text.lines().enumerate().any(|(i, line)| {
                let n = i + 1;
                let own =
                    path.as_str() == now.file_path && n >= now.start_line && n <= now.end_line;
                !own && mentions(line, &now.name)
            })
        });
        if still_named {
            continue;
        }
        let span = Span {
            path: now.file_path.clone(),
            start_line: now.start_line,
            end_line: now.end_line,
        };
        let public = now.visibility.starts_with("pub") || now.visibility == "export";
        let mut evidence: Vec<String> = callers
            .iter()
            .take(3)
            .map(|c| format!("called before this change from {c}"))
            .collect();
        evidence.push(format!(
            "`{}` appears nowhere else in the project's {} files (git-tracked and untracked, \
             any type)",
            now.name,
            texts.len()
        ));
        out.push(Finding {
            id: format!("orphan:{}:{}", now.file_path, now.qualified_name),
            rule: "orphan",
            severity: Severity::Low,
            symbol: now.qualified_name.clone(),
            location: loc(&span),
            message: format!(
                "`{}` lost its last caller in this change and looks unused{}: confirm (dynamic \
                 dispatch, reflection and other repositories are not visible here), then \
                 delete it, or restore the call if dropping it was a mistake",
                now.qualified_name,
                if public {
                    " in this project (it is public: code outside the project may still use it)"
                } else {
                    ""
                }
            ),
            evidence,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn mentions_needs_whole_identifiers() {
        assert!(mentions("let f = helper;", "helper"));
        assert!(mentions("helper(1)", "helper"));
        assert!(!mentions("helper_two(1)", "helper"));
        assert!(!mentions("my_helper()", "helper"));
        assert!(mentions("x.helper()", "helper"));
    }
}
