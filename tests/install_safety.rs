//! Installer writers never lose the content of a file they could not read.

use tempfile::TempDir;

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

fn decode_utf16le(bytes: &[u8]) -> String {
    assert_eq!(
        &bytes[..2],
        &[0xFF, 0xFE],
        "the UTF-16 byte order mark was dropped"
    );
    let units: Vec<u16> = bytes[2..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect();
    String::from_utf16(&units).unwrap()
}

/// A UTF-16 CLAUDE.md gets the section appended and stays UTF-16; the first backup holds
/// the original bytes.
#[test]
fn utf16_claude_md_keeps_its_text_and_encoding() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let original = utf16le_with_bom("# My rules\nNever push to main.\n");
    write(root, "CLAUDE.md", &original);

    trackertop::install::install_claude_code(root, false).unwrap();

    let text = decode_utf16le(&std::fs::read(root.join("CLAUDE.md")).unwrap());
    assert!(
        text.starts_with("# My rules\nNever push to main.\n"),
        "{text}"
    );
    assert!(text.contains("TrackerTop MCP Navigation"), "{text}");
    assert_eq!(
        std::fs::read(root.join("CLAUDE.md.trackertop.bak")).unwrap(),
        original
    );

    // Running again changes nothing
    let before = std::fs::read(root.join("CLAUDE.md")).unwrap();
    trackertop::install::install_claude_code(root, false).unwrap();
    assert_eq!(std::fs::read(root.join("CLAUDE.md")).unwrap(), before);
}

/// UTF-8 with a byte order mark keeps the mark; a plain UTF-8 file gets a backup too.
#[test]
fn utf8_claude_md_is_backed_up_and_keeps_its_bom() {
    for (body, bom) in [
        (b"\xEF\xBB\xBF# Rules\n".to_vec(), true),
        (b"# Rules\n".to_vec(), false),
    ] {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "CLAUDE.md", &body);
        trackertop::install::install_claude_code(root, false).unwrap();
        let after = std::fs::read(root.join("CLAUDE.md")).unwrap();
        assert_eq!(after.starts_with(b"\xEF\xBB\xBF# Rules\n"), bom);
        assert!(String::from_utf8_lossy(&after).contains("TrackerTop MCP Navigation"));
        assert_eq!(
            std::fs::read(root.join("CLAUDE.md.trackertop.bak")).unwrap(),
            body
        );
    }
}

/// Content that is neither UTF-8 nor BOM-marked UTF-16 (here Latin-1) stops the install
/// with an error that names the file, and the file is left byte for byte as it was.
#[test]
fn unreadable_claude_md_stops_the_install_untouched() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let original = b"# R\xE8gles\nNe pousse jamais sur main.\n".to_vec();
    write(root, "CLAUDE.md", &original);

    let err = trackertop::install::install_claude_code(root, false).unwrap_err();
    assert!(format!("{err:#}").contains("CLAUDE.md"), "{err:#}");
    assert_eq!(std::fs::read(root.join("CLAUDE.md")).unwrap(), original);
}

/// The project `.mcp.json` follows the same rules: a UTF-16 file stays UTF-16 and keeps its
/// other servers; an unreadable one is refused and left alone.
#[test]
fn project_mcp_json_keeps_utf16_and_refuses_unreadable() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let original = utf16le_with_bom(r#"{"mcpServers":{"other":{"command":"x"}}}"#);
    write(root, ".mcp.json", &original);
    trackertop::install::install_claude_code(root, false).unwrap();
    let text = decode_utf16le(&std::fs::read(root.join(".mcp.json")).unwrap());
    assert!(
        text.contains("\"other\"") && text.contains("\"trackertop\""),
        "{text}"
    );
    assert_eq!(
        std::fs::read(root.join(".mcp.json.trackertop.bak")).unwrap(),
        original
    );

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let original = b"{\"note\":\"caf\xE9\"}".to_vec();
    write(root, ".mcp.json", &original);
    assert!(trackertop::install::install_claude_code(root, false).is_err());
    assert_eq!(std::fs::read(root.join(".mcp.json")).unwrap(), original);
    assert!(!root.join("CLAUDE.md").exists());
}
