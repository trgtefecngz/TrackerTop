//! Memory tools: remember (notes, decisions, dismissed leads) and recall.

use super::*;

impl TrackerEngine {
    /// 13. Persists a note, decision or dismissed review lead attached to a symbol, file, or
    ///     `"project"`. Decisions and dismissed leads require a reason.
    pub fn remember(&self, target: &str, note: &str) -> Result<String> {
        self.remember_kind(target, note, "note", "")
    }

    pub fn remember_kind(
        &self,
        target: &str,
        note: &str,
        kind: &str,
        reason: &str,
    ) -> Result<String> {
        let (db, cg) = self.load_fresh()?;
        // Provenance: which commit, and a hash of the target's code, so a later read can
        // flag the note when that code has changed
        let commit = crate::changes::git(&self.repo_root, &["rev-parse", "--short=12", "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let code_hash = self.note_code_hash(&cg, target).unwrap_or_default();
        let (id, mirror_err) =
            db.add_note_with_provenance(target, note, kind, reason, &commit, &code_hash)?;
        let target = if target.trim().is_empty() {
            "project"
        } else {
            target.trim()
        };
        let mut out = format!(
            "Saved {kind} #{id} for `{target}`. It is shown automatically when that symbol or \
             file is read (read_symbol, read_symbols_batch, file_outline) and in recall."
        );
        if let Some(e) = mirror_err {
            out.push_str(&format!(
                "\nWARNING: stored in the index DB, but the Markdown copy in .trackertop/notes/ \
                 was not written ({e}); it will not survive an index rebuild."
            ));
        }
        Ok(out)
    }

    /// Hash of the code a note target names, as it is now: a review finding id
    /// (`rule:path:symbol`), an indexed file path, or a symbol. `None` when the target is
    /// not code (`project`) or cannot be found.
    pub(super) fn note_code_hash(&self, cg: &CodeGraph, target: &str) -> Option<String> {
        let t = target.trim().replace('\\', "/");
        if t.is_empty() || t.eq_ignore_ascii_case("project") {
            return None;
        }
        let hash = |text: &str| blake3::hash(text.as_bytes()).to_hex()[..16].to_string();
        let file_text = |path: &str| std::fs::read_to_string(self.repo_root.join(path)).ok();
        let symbol_text = |s: &Symbol| {
            let text = file_text(&s.file_path)?;
            Some(
                text.lines()
                    .skip(s.start_line.saturating_sub(1))
                    .take(s.end_line + 1 - s.start_line.min(s.end_line + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        // A review finding id: rule:path:qualified_name
        let mut parts = t.splitn(3, ':');
        if let (Some(_rule), Some(path), Some(qname)) = (parts.next(), parts.next(), parts.next())
            && !path.is_empty()
            && (cg.files.contains_key(path) || self.repo_root.join(path).is_file())
        {
            let sym = cg.symbols_by_file.get(path).and_then(|ids| {
                ids.iter()
                    .filter_map(|id| cg.symbols.get(id))
                    .find(|s| s.qualified_name == qname)
            });
            return match sym {
                Some(s) => symbol_text(s).map(|x| hash(&x)),
                // The symbol is gone (a removed function): its file stands in for it
                None => file_text(path).map(|x| hash(&x)),
            };
        }
        if cg.files.contains_key(&t) {
            return file_text(&t).map(|x| hash(&x));
        }
        // Exact only: a fuzzy guess would hash some other symbol's code
        let s = cg.resolve_exact(&t)?;
        symbol_text(s).map(|x| hash(&x))
    }

    /// A note as shown next to code: its text, plus a warning when the code it was written
    /// about has changed since, so an old decision or dismissed lead is re-checked rather
    /// than trusted.
    pub(super) fn note_line(&self, cg: &CodeGraph, n: &ProjectNote) -> String {
        let mut line = n.display_line();
        if n.code_hash.is_empty() {
            return line;
        }
        let at = if n.commit.is_empty() {
            String::new()
        } else {
            format!(" at commit {}", n.commit)
        };
        match self.note_code_hash(cg, &n.target) {
            Some(now) if now == n.code_hash => {}
            Some(_) => line.push_str(&format!(
                " [POSSIBLY OUTDATED: the code changed since this note was written{at}; \
                 re-check before relying on it]"
            )),
            None => line.push_str(&format!(
                " [POSSIBLY OUTDATED: `{}` is no longer found (renamed or removed?); written{at}]",
                n.target
            )),
        }
        line
    }

    /// Whether the code a note was written about is unchanged (true when the note has no
    /// recorded hash: nothing to compare).
    pub(super) fn note_still_current(&self, cg: &CodeGraph, n: &ProjectNote) -> bool {
        n.code_hash.is_empty()
            || self.note_code_hash(cg, &n.target).as_deref() == Some(&n.code_hash)
    }

    /// 14. Recalls saved notes, decisions and dismissed leads (all of them; no silent cap).
    pub fn recall(&self, query: Option<&str>) -> Result<String> {
        let (db, cg) = self.load_fresh()?;
        let notes = db.list_notes(query)?;
        if notes.is_empty() {
            return Ok(match query {
                Some(q) if !q.trim().is_empty() => {
                    format!("No saved notes match `{q}` (searched target, text, reason and kind).")
                }
                _ => "No saved memory notes yet. Use `remember(target, note)` to store \
                      architectural facts, decisions and dismissed review leads."
                    .to_string(),
            });
        }
        let mut out = format!("# SAVED MEMORY NOTES ({})\n", notes.len());
        for n in notes {
            out.push_str(&format!("- {}\n", self.note_line(&cg, &n)));
        }
        Ok(out)
    }
}
