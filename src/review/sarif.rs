//! SARIF 2.1.0 output, for code-scanning UIs (GitHub, IDEs). Each finding keeps its
//! stable id as a partial fingerprint, so a UI tracks it across runs like the review does.

use super::ChangeSet;
use super::rules::{Finding, Severity};
use serde_json::{Value, json};

/// `path:Lstart-end` or `path:line` -> (path, start, end). A bare path has no region.
fn parse_location(loc: &str) -> (String, Option<(usize, usize)>) {
    if let Some((path, range)) = loc.rsplit_once(":L") {
        let (a, b) = range.split_once('-').unwrap_or((range, range));
        if let (Ok(a), Ok(b)) = (a.parse(), b.parse()) {
            return (path.to_string(), Some((a, b)));
        }
    }
    if let Some((path, line)) = loc.rsplit_once(':')
        && let Ok(n) = line.parse()
    {
        return (path.to_string(), Some((n, n)));
    }
    (loc.to_string(), None)
}

/// Where a finding points in the working tree. A removed symbol's own location exists only
/// at the base, so its first `file:line` evidence (the caller still using it) is used.
fn head_location(f: &Finding) -> Option<(String, Option<(usize, usize)>)> {
    if f.location.starts_with("was ") {
        let first = f.evidence.first()?;
        let spot = first.split_whitespace().next()?.trim_end_matches(':');
        let (path, region) = parse_location(spot);
        return region.map(|r| (path, Some(r)));
    }
    Some(parse_location(&f.location))
}

fn level(s: Severity) -> &'static str {
    match s {
        Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

/// `prefix` is where the project sits in the repository (`""`, or `packages/app/` in a
/// monorepo): review paths are project-relative, SARIF consumers read them from the
/// repository root.
pub fn render_sarif(cs: &ChangeSet, prefix: &str) -> Value {
    let mut rules: Vec<&str> = cs.findings.iter().map(|f| f.rule).collect();
    rules.sort_unstable();
    rules.dedup();
    let results: Vec<Value> = cs
        .findings
        .iter()
        .map(|f| {
            let mut text = f.message.clone();
            for e in &f.evidence {
                text.push_str("\n- ");
                text.push_str(e);
            }
            let mut r = json!({
                "ruleId": f.rule,
                "level": level(f.severity),
                "message": { "text": text },
                "partialFingerprints": { "trackertopFindingId": f.id },
            });
            if let Some((path, region)) = head_location(f) {
                let mut phys = json!({ "artifactLocation": { "uri": format!("{prefix}{path}") } });
                if let Some((a, b)) = region {
                    phys["region"] = json!({ "startLine": a, "endLine": b.max(a) });
                }
                r["locations"] = json!([{ "physicalLocation": phys }]);
            }
            r
        })
        .collect();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": {
                "name": "TrackerTop",
                "version": env!("CARGO_PKG_VERSION"),
                "informationUri": "https://github.com/trgtefecngz/TrackerTop",
                "rules": rules.iter().map(|r| json!({ "id": r })).collect::<Vec<_>>(),
            }},
            // What was analyzed is the working tree (HEAD plus uncommitted work), compared
            // with the base: no `versionControlProvenance`, which would name one revision
            "properties": {
                "analyzed": "working tree (HEAD plus uncommitted work)",
                "baseRef": cs.base_ref,
                "baseCommit": cs.base_commit,
            },
            "results": results,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(location: &str, evidence: &[&str]) -> Finding {
        Finding {
            id: "r:p:s".into(),
            rule: "broken-ref",
            severity: Severity::High,
            symbol: "s".into(),
            location: location.into(),
            message: "m".into(),
            evidence: evidence.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn locations_point_into_the_working_tree() {
        assert_eq!(
            head_location(&finding("src/a.rs:L3-9", &[])),
            Some(("src/a.rs".into(), Some((3, 9))))
        );
        assert_eq!(
            head_location(&finding("src/a.rs", &[])),
            Some(("src/a.rs".into(), None))
        );
        // Removed symbol: the caller that still uses it
        assert_eq!(
            head_location(&finding(
                "was src/a.rs:L2-2",
                &["src/b.rs:5 in caller: `old()`"]
            )),
            Some(("src/b.rs".into(), Some((5, 5))))
        );
        assert_eq!(head_location(&finding("was src/a.rs:L2-2", &[])), None);
    }

    #[test]
    fn uris_are_repository_relative_in_a_monorepo_package() {
        let cs = ChangeSet {
            base_ref: "main".into(),
            base_commit: "abc".into(),
            files: vec![],
            symbols: vec![],
            findings: vec![finding("src/a.rs:L3-9", &[])],
        };
        let v = render_sarif(&cs, "packages/app/");
        let loc = &v["runs"][0]["results"][0]["locations"][0]["physicalLocation"];
        assert_eq!(loc["artifactLocation"]["uri"], "packages/app/src/a.rs");
        assert_eq!(loc["region"]["startLine"], 3);
        // The working tree was analyzed, not the base commit
        assert!(v["runs"][0].get("versionControlProvenance").is_none());
        assert_eq!(v["runs"][0]["properties"]["baseCommit"], "abc");
    }
}
