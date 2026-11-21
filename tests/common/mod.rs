//! Helpers shared by the integration test files (each file is its own crate and uses a
//! subset, hence the allow).
#![allow(dead_code)]

pub fn write_call_graph_repo(root: &std::path::Path, extra_fns: usize) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    let mut b = String::from("pub fn leaf() -> u8 { 1 }\n");
    for k in 0..extra_fns {
        b.push_str(&format!("pub fn extra_{k}() -> u8 {{ leaf() }}\n"));
    }
    std::fs::write(root.join("src/b.rs"), b).unwrap();
    let mut a = String::from("mod b;\npub fn root() -> u8 { b::leaf() }\n");
    for k in 0..extra_fns {
        a.push_str(&format!("pub fn call_{k}() -> u8 {{ b::extra_{k}() }}\n"));
    }
    std::fs::write(root.join("src/lib.rs"), a).unwrap();
}

/// Opens the index of an already-indexed repo WITHOUT creating it, so a moved or renamed
/// store fails with a clear message instead of an empty database and "no such table".
pub fn open_existing_index(root: &std::path::Path) -> rusqlite::Connection {
    let path = root.join(".trackertop/index.db");
    assert!(
        path.is_file(),
        "index store not found at {}",
        path.display()
    );
    rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap()
}

/// Sorted `(source, target, kind)` rows of the persisted edge set.
pub fn edge_set(root: &std::path::Path) -> Vec<(String, String, String)> {
    let conn = open_existing_index(root);
    let mut stmt = conn
        .prepare("SELECT source_id, target_id, kind FROM edges")
        .unwrap();
    let mut rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}

/// All regular files under `root`, excluding TrackerTop's own `.trackertop/` store.
pub fn walk_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == ".trackertop") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// Ground truth for smart_grep: an independent scan of every file on disk whose type the
/// registry indexes, splitting on '\n' and dropping a trailing '\r' (no `str::lines`).
pub fn naive_grep(root: &std::path::Path, pattern: &str) -> Vec<(String, usize)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                if !p.file_name().unwrap().to_string_lossy().starts_with('.') {
                    walk(&p, out);
                }
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(root, &mut files);
    let pat = pattern.to_lowercase();
    let registry = trackertop::parser::default_registry();
    let mut hits = Vec::new();
    for f in files {
        if registry.extractor_for_path(&f).is_none() {
            continue;
        }
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&f).unwrap();
        for (i, raw) in text.split('\n').enumerate() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.to_lowercase().contains(&pat) {
                hits.push((rel.clone(), i + 1));
            }
        }
    }
    hits.sort();
    hits
}

pub fn git_in(root: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Sorted `(source, target, kind, confidence)` rows of the persisted edge set.
pub fn labelled_edges(root: &std::path::Path) -> Vec<(String, String, String, String)> {
    let conn = open_existing_index(root);
    let mut stmt = conn
        .prepare("SELECT source_id, target_id, kind, confidence FROM edges")
        .unwrap();
    let mut rows: Vec<(String, String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}
