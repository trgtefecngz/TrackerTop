//! Reference resolution: which call and type edges the indexer creates, and which it refuses.

mod common;

use common::*;
use tempfile::TempDir;
use trackertop::indexer::index_repository;

/// Qualified Rust calls must resolve to the named type or module, never to an unrelated
/// symbol that merely shares the last segment (`Stroke::new` is not `App::new`, and
/// `std::fs::read` is not this project's `util::read`). Missing an edge is better than a
/// wrong one.
#[test]
fn qualified_rust_calls_never_bind_to_a_namesake() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "mod app;\nmod util;\n").unwrap();
    std::fs::write(
        root.join("src/util.rs"),
        "pub fn helper() {}\npub fn read() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app.rs"),
        r#"pub struct App;
impl App {
    pub fn new() -> Self { App }
    pub fn build() -> Self { Self::new() }
}
pub fn make_app() -> App { App::new() }
pub fn external_ctor() { let _s = Stroke::new(1.0); }
pub fn std_read() { let _ = std::fs::read("x"); }
pub fn module_call() { util::helper(); crate::util::helper(); }
pub fn variant() { let _ = Kind::New(1); }
"#,
    )
    .unwrap();
    index_repository(root, true).unwrap();

    let calls: Vec<(String, String)> = edge_set(root)
        .into_iter()
        .filter(|(_, _, k)| k == "CALLS")
        .map(|(s, t, _)| (s, t))
        .collect();
    let from = |f: &str| -> Vec<&str> {
        calls
            .iter()
            .filter(|(s, _)| s.contains(&format!("::{f}#")))
            .map(|(_, t)| t.as_str())
            .collect()
    };

    assert_eq!(from("make_app").len(), 1, "{calls:?}");
    assert!(from("make_app")[0].contains("::App::new#"), "{calls:?}");
    assert_eq!(from("App::build").len(), 1, "{calls:?}");
    assert!(from("App::build")[0].contains("::App::new#"), "{calls:?}");
    assert!(from("external_ctor").is_empty(), "{calls:?}");
    assert!(from("std_read").is_empty(), "{calls:?}");
    assert!(from("variant").is_empty(), "{calls:?}");
    let module = from("module_call");
    assert_eq!(module.len(), 1, "{calls:?}");
    assert!(module[0].contains("src/util.rs::helper#"), "{calls:?}");
}

/// "Who uses this type": Rust function bodies that match on an enum, build a struct or
/// annotate a binding with it are type users; a function sharing the type's name is not
/// a type, and an external type gets no edge.
#[test]
fn rust_type_users_include_function_bodies() {
    use trackertop::graph::CodeGraph;
    use trackertop::indexer::index_repository;
    use trackertop::model::EdgeKind;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub enum Level { Low, High }\n\
         pub struct Rec { pub a: i32 }\n\
         #[allow(non_snake_case)]\npub fn Widget() -> i32 { 1 }\n\
         pub fn by_match(x: i32) -> i32 {\n    let l = if x > 0 { Level::High } else { Level::Low };\n    match l {\n        Level::High => 1,\n        Level::Low => 0,\n    }\n}\n\
         pub fn builds() -> i32 {\n    let r = Rec { a: 1 };\n    r.a\n}\n\
         pub fn annotated() -> usize {\n    let v: Vec<Rec> = Vec::new();\n    v.len()\n}\n\
         pub fn external() -> u64 {\n    let w: Widget = Widget::default();\n    std::time::Duration::from_secs(1).as_secs()\n}\n",
    )
    .unwrap();
    index_repository(root, false).unwrap();
    let db = trackertop::db::Database::open_for_repo(root).unwrap();
    let cg = CodeGraph::load(&db).unwrap();
    let users = |ty: &str| -> Vec<String> {
        let mut v: Vec<String> = cg
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::UsesType)
            .filter(|e| cg.symbols.get(&e.target_id).is_some_and(|s| s.name == ty))
            .filter_map(|e| cg.symbols.get(&e.source_id).map(|s| s.name.clone()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(users("Level"), vec!["by_match"]);
    assert_eq!(users("Rec"), vec!["annotated", "builds"]);
    // `Widget` here is an external type: no edge to the function of that name
    assert!(users("Widget").is_empty(), "{:?}", users("Widget"));
}

/// A struct-like enum variant (`Msg::Move { x }`) or an associated type (`T::Output`,
/// `Self::Item`) is not a use of an unrelated type that shares the name.
#[test]
fn rust_variant_and_associated_names_are_not_type_uses() {
    use trackertop::graph::CodeGraph;
    use trackertop::indexer::index_repository;
    use trackertop::model::EdgeKind;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub enum Msg { Move { x: i32 }, Quit }\npub struct Move;\npub struct Output;\n\
         pub fn handle(m: Msg) -> i32 {\n    match m {\n        Msg::Move { x } => x,\n        Msg::Quit => 0,\n    }\n}\n\
         pub fn assoc<T: std::ops::Add>(a: T, b: T) -> i32 {\n    let _y: T::Output = a + b;\n    0\n}\n\
         pub mod inner {\n    pub struct Deep;\n}\npub fn modpath() {\n    let _d: inner::Deep = inner::Deep;\n}\n",
    )
    .unwrap();
    index_repository(root, false).unwrap();
    let db = trackertop::db::Database::open_for_repo(root).unwrap();
    let cg = CodeGraph::load(&db).unwrap();
    let targets_of = |src: &str| -> Vec<String> {
        let mut v: Vec<String> = cg
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::UsesType)
            .filter(|e| cg.symbols.get(&e.source_id).is_some_and(|s| s.name == src))
            .filter_map(|e| cg.symbols.get(&e.target_id).map(|s| s.name.clone()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(targets_of("handle"), vec!["Msg"]);
    assert!(targets_of("assoc").is_empty(), "{:?}", targets_of("assoc"));
    // A module path names the type at its end
    assert_eq!(targets_of("modpath"), vec!["Deep"]);
}

/// A Rust method call on a value reaches only a project method that takes `self`, and a
/// call on a standard iterator chain (`s.chars().count()`) reaches no project method at
/// all: these false edges once tied `src/parser/*` to `ChangeSet::count` in `src/review`.
/// A value-method name that several other types define (`s.parse()` vs. every extractor's
/// `parse`) is ambiguous and gets no edge, and a local closure shadows a namesake function.
#[test]
fn rust_method_calls_on_std_values_never_bind_to_a_namesake() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/other.rs"),
        r#"pub struct A;
pub struct B;
impl A { pub fn parse(&self) -> u8 { 1 } }
impl B { pub fn parse(&self) -> u8 { 2 } }
pub fn base_name(p: &str) -> &str { p }
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        r#"mod other;
pub fn num(s: &str) -> u8 { s.parse().unwrap_or(0) }
pub fn shadow(p: &str) -> usize { let base_name = |s: &str| s.len(); base_name(p) }
pub fn real_free(p: &str) -> &str { other::base_name(p) }
pub struct Set { n: usize }
impl Set {
    pub fn count(&self) -> usize { self.n }
    pub fn all() -> Vec<Set> { Vec::new() }
}
pub fn letters(s: &str) -> usize { s.chars().filter(|c| c.is_alphabetic()).count() }
pub fn lines_in(s: &str) -> usize { s.lines().count().max(1) }
pub fn hashes(s: &str) -> usize { s.chars().take_while(|c| *c == '#').count() }
pub fn all_set(mut it: impl Iterator<Item = u8>) -> bool { it.all(|b| b > 0) }
pub fn in_macro(s: &str) { assert_eq!(s.lines().count(), 1); }
pub fn real(s: &Set) -> usize { s.count() }
pub fn real_in_macro(s: &Set) { assert_eq!(s.count(), 0); }
pub fn real_assoc() -> usize { Set::all().len() }
"#,
    )
    .unwrap();
    index_repository(root, true).unwrap();

    let calls: Vec<(String, String)> = edge_set(root)
        .into_iter()
        .filter(|(_, _, k)| k == "CALLS")
        .map(|(s, t, _)| (s, t))
        .collect();
    let from = |f: &str| -> Vec<&str> {
        calls
            .iter()
            .filter(|(s, _)| s.contains(&format!("::{f}#")))
            .map(|(_, t)| t.as_str())
            .collect()
    };
    assert_eq!(from("real_free").len(), 1, "{calls:?}");
    for f in [
        "letters", "lines_in", "hashes", "all_set", "in_macro", "num", "shadow",
    ] {
        assert!(from(f).is_empty(), "{f}: {calls:?}");
    }
    assert_eq!(from("real").len(), 1, "{calls:?}");
    assert!(from("real")[0].contains("::Set::count#"), "{calls:?}");
    assert_eq!(from("real_in_macro").len(), 1, "{calls:?}");
    assert_eq!(from("real_assoc").len(), 1, "{calls:?}");
    assert!(from("real_assoc")[0].contains("::Set::all#"), "{calls:?}");
}

/// Every resolved edge says how it was found: a qualified path, `Self::`, a module path or
/// a name defined once in the caller's own file is `exact`; a bare name found only in
/// other files, a name with several candidates, or a method call on a value is
/// `heuristic`. Heuristic edges are kept, never dropped.
#[test]
fn edges_are_labelled_exact_or_heuristic() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "mod app;\nmod util;\nmod more;\n").unwrap();
    std::fs::write(
        root.join("src/util.rs"),
        "pub fn helper() {}\npub fn only_there() {}\npub fn twice() {}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/more.rs"), "pub fn twice() {}\n").unwrap();
    std::fs::write(
        root.join("src/app.rs"),
        r#"pub struct App { n: usize }
impl App {
    pub fn new() -> Self { App { n: 0 } }
    pub fn build() -> Self { Self::new() }
    pub fn size(&self) -> usize { self.n }
}
fn local() {}
pub fn by_type() -> App { App::new() }
pub fn by_module() { util::helper(); }
pub fn by_local() { local(); }
pub fn by_far_name() { only_there(); }
pub fn by_shared_name() { twice(); }
pub fn by_method(a: &App) -> usize { a.size() }
"#,
    )
    .unwrap();
    index_repository(root, true).unwrap();

    let calls: Vec<(String, String, String)> = labelled_edges(root)
        .into_iter()
        .filter(|(_, _, k, _)| k == "CALLS")
        .map(|(s, t, _, c)| (s, t, c))
        .collect();
    let label = |f: &str| -> Vec<&str> {
        calls
            .iter()
            .filter(|(s, _, _)| s.contains(&format!("::{f}#")))
            .map(|(_, _, c)| c.as_str())
            .collect()
    };
    for (f, want) in [
        ("by_type", "exact"),
        ("App::build", "exact"),
        ("by_module", "exact"),
        ("by_local", "exact"),
        ("by_far_name", "heuristic"),
        ("by_shared_name", "heuristic"),
        ("by_method", "heuristic"),
    ] {
        assert_eq!(label(f), vec![want], "{f}: {calls:?}");
    }
    // Module declarations resolve by path
    let imports: Vec<_> = labelled_edges(root)
        .into_iter()
        .filter(|(s, _, k, _)| k == "IMPORTS" && s.starts_with("src/lib.rs"))
        .collect();
    assert_eq!(imports.len(), 3, "{imports:?}");
    assert!(imports.iter().all(|e| e.3 == "exact"), "{imports:?}");
}
