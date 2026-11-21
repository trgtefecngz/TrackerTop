//! Failing tests for confirmed items of the external v0.8.2 review (docs/FEEDBACK_TRIAGE.md).
//! Each test is named after its triage id. They fail on v0.8.2 on purpose: the fix for an
//! item is done when its test passes. Tests only; no fixes on this branch.

use tempfile::TempDir;
use trackertop::context::TrackerEngine;

fn write(root: &std::path::Path, rel: &str, body: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut v = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

/// A1: `install claude` must never drop the content of a CLAUDE.md it cannot read as UTF-8
/// (here UTF-16, as PowerShell 5.1 `Out-File` writes). Either it refuses, or the original
/// bytes survive in the file or in a backup next to it.
#[test]
fn a1_install_keeps_utf16_claude_md() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let original = utf16le_with_bom("# My rules\nNever push to main.\n");
    write(root, "CLAUDE.md", &original);

    let _ = trackertop::install::install_claude_code(root, false);

    let survived = std::fs::read_dir(root).unwrap().flatten().any(|e| {
        e.path().is_file()
            && std::fs::read(e.path())
                .map(|b| b.windows(original.len()).any(|w| w == original.as_slice()))
                .unwrap_or(false)
    });
    assert!(
        survived,
        "the user's UTF-16 CLAUDE.md content is gone and no backup holds it"
    );
}

/// A2: a file that stops being readable as UTF-8 must not keep serving its old symbols.
#[test]
fn a2_non_utf8_edit_drops_stale_symbols() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/lib.rs", b"pub fn old_name() -> u32 { 1 }\n");
    let engine = TrackerEngine::new(root);
    assert!(
        !engine
            .read_symbol("old_name", false, 0)
            .unwrap()
            .text
            .contains("NOT FOUND")
    );

    // Rename plus one Latin-1 byte (0xE9) in a comment: no longer valid UTF-8
    write(
        root,
        "src/lib.rs",
        b"// caf\xe9\npub fn new_name() -> u32 { 1 }\npub fn more() -> u32 { 2 }\n",
    );
    let out = engine.read_symbol("old_name", false, 0).unwrap().text;
    assert!(
        !out.starts_with("# SYMBOL old_name"),
        "removed function is still served from the index:\n{out}"
    );
}

/// A3: a call to the user's own method named like a std method (`insert`, `get`) must
/// count as a dependent; otherwise impact says 0 and review says "untested".
#[test]
fn a3_std_named_user_method_has_dependents() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "src/lib.rs",
        b"pub struct Store { v: Vec<u32> }\n\
impl Store {\n    pub fn insert(&mut self, x: u32) { self.v.push(x); }\n}\n\
pub fn use_store(s: &mut Store) { s.insert(3); }\n",
    );
    let out = TrackerEngine::new(root)
        .impact_analysis("Store::insert", 1, 0)
        .unwrap()
        .text;
    assert!(out.contains("use_store"), "caller missing:\n{out}");
}

/// A4: a module imported under an alias (`use crate::theme as T;`) must still link calls
/// made through the alias.
#[test]
fn a4_aliased_module_calls_resolve() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/lib.rs", b"pub mod theme;\npub mod a;\n");
    write(root, "src/theme.rs", b"pub fn shade() -> u32 { 1 }\n");
    write(
        root,
        "src/a.rs",
        b"use crate::theme as T;\npub fn paint() -> u32 { T::shade() }\n",
    );
    let out = TrackerEngine::new(root)
        .impact_analysis("shade", 1, 0)
        .unwrap()
        .text;
    assert!(out.contains("paint"), "call through alias missing:\n{out}");
}

/// A5: a supported file that cannot be decoded (UTF-16) must be either indexed or named as
/// skipped; `find` must not answer a plain "No symbols matched".
#[test]
fn a5_utf16_file_is_indexed_or_reported() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "tools.ps1",
        &utf16le_with_bom("function Get-Sixteen {\n  param($x)\n  $x\n}\n"),
    );
    write(root, "src/lib.rs", b"pub fn x() {}\n");
    let out = TrackerEngine::new(root)
        .find_symbol("Get-Sixteen", 5, 0)
        .unwrap()
        .text;
    assert!(
        out.contains("Get-Sixteen [") || out.contains("tools.ps1"),
        "UTF-16 file silently missing:\n{out}"
    );
}

/// New finding N2: a UTF-8 BOM hides the first PowerShell function.
#[test]
fn n2_powershell_bom_keeps_first_function() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "tools.ps1",
        b"\xEF\xBB\xBFfunction Get-Bom {\n  param($x)\n  $x\n}\n",
    );
    let out = TrackerEngine::new(root)
        .find_symbol("Get-Bom", 5, 0)
        .unwrap()
        .text;
    assert!(out.contains("Get-Bom ["), "BOM hid the function:\n{out}");
}

/// A9: the whole answer, footer included, stays within `max_tokens`.
#[test]
fn a9_small_budget_is_respected() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let mut body = String::from("pub fn big() -> u32 {\n");
    for i in 0..200 {
        body.push_str(&format!("    let v{i} = {i} + 1;\n"));
    }
    body.push_str("    0\n}\n");
    write(root, "src/lib.rs", body.as_bytes());
    let out = TrackerEngine::new(root)
        .read_symbol("big", false, 60)
        .unwrap();
    assert!(
        out.tokens_sent <= 60,
        "sent {} tokens for max_tokens=60",
        out.tokens_sent
    );
}

/// A11: a `build/` folder that .gitignore does not ignore is source and must be indexed.
#[test]
fn a11_unignored_build_folder_is_indexed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "build/helper.py",
        b"def build_helper():\n    return 1\n",
    );
    write(root, "src/lib.rs", b"pub fn x() {}\n");
    let out = TrackerEngine::new(root)
        .file_outline("build/helper.py", 0)
        .unwrap()
        .text;
    assert!(out.contains("build_helper"), "build/ was skipped:\n{out}");
}

/// A18: `depth=1` must not silently list 2-hop dependents.
#[test]
fn a18_impact_depth_one_is_one_hop() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "src/lib.rs",
        b"pub fn leaf() -> u8 { 1 }\npub fn mid() -> u8 { leaf() }\npub fn top() -> u8 { mid() }\n",
    );
    let out = TrackerEngine::new(root)
        .impact_analysis("leaf", 1, 0)
        .unwrap()
        .text;
    assert!(out.contains("mid"), "{out}");
    assert!(
        !out.contains("- top ["),
        "depth=1 listed a 2-hop dependent:\n{out}"
    );
}

/// A7: a truncated read must not be logged as a near-total saving of whole files.
#[test]
fn a7_truncated_read_is_not_logged_as_full_saving() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let mut body = String::from("pub fn big() -> u32 {\n");
    for i in 0..300 {
        body.push_str(&format!("    let v{i} = {i} + 1;\n"));
    }
    body.push_str("    0\n}\n");
    write(root, "src/lib.rs", body.as_bytes());
    let out = TrackerEngine::new(root)
        .read_symbol("big", false, 80)
        .unwrap();
    // Only a few of ~300 lines were sent: the baseline must not be the full symbol/file
    assert!(
        out.baseline_tokens <= out.tokens_sent * 2,
        "truncated read logged baseline {} vs sent {}",
        out.baseline_tokens,
        out.tokens_sent
    );
}
