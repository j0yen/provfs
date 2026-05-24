//! Integration tests for the provfs public API.
//!
//! We can't easily mount a real FUSE in `cargo test` (needs CAP_SYS_ADMIN
//! or fuser kernel module + privileges), so these tests cover the parts
//! of the public surface that compose the stamping pipeline end-to-end:
//! identity resolution, skip-list routing, and xattr stamping on a
//! real temp directory backed by tmpfs.

use std::path::Path;
use tempfile::TempDir;

use provfs::xattrs;
use provfs::{Identity, SkipList};

#[test]
fn stamp_round_trips_through_xattrs() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("note.md");
    std::fs::write(&file, b"hello").unwrap();

    let id = Identity {
        session: "01TESTSESSIONABCDEF".to_string(),
        tool: "Edit".to_string(),
        turn: Some("42".to_string()),
        intent: Some("autobuilder".to_string()),
    };

    // Skip if filesystem doesn't support user xattrs (e.g. some tmpfs configs).
    if xattrs::stamp(&file, &id, "2026-05-24T15:00:00Z").is_err() {
        eprintln!("skipping: xattrs unsupported on this tmp filesystem");
        return;
    }

    let read_back = |k: &str| -> Option<String> {
        xattr::get(&file, k)
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v).ok())
    };

    assert_eq!(read_back(xattrs::KEY_SESSION).as_deref(), Some(id.session.as_str()));
    assert_eq!(read_back(xattrs::KEY_TOOL).as_deref(), Some("Edit"));
    assert_eq!(read_back(xattrs::KEY_TURN).as_deref(), Some("42"));
    assert_eq!(read_back(xattrs::KEY_INTENT).as_deref(), Some("autobuilder"));
    assert_eq!(read_back(xattrs::KEY_TS).as_deref(), Some("2026-05-24T15:00:00Z"));
    assert_eq!(read_back(xattrs::KEY_HISTORY).as_deref(), Some(id.session.as_str()));
}

#[test]
fn skiplist_defaults_skip_git_target_node_modules() {
    let s = SkipList::defaults();
    for p in [".git/HEAD", ".git/objects/00/aabb", "target/release/x", "node_modules/foo"] {
        assert!(s.should_skip(Path::new(p)), "expected skip: {p}");
    }
    for p in ["README.md", "src/lib.rs", "docs/index.html"] {
        assert!(!s.should_skip(Path::new(p)), "expected pass-through: {p}");
    }
}

#[test]
fn user_spec_extends_defaults() {
    let s = SkipList::from_user_spec("private/, secrets/");
    assert!(s.should_skip(Path::new("private/notes.md")));
    assert!(s.should_skip(Path::new("secrets/api.key")));
    // Defaults still apply.
    assert!(s.should_skip(Path::new(".git/HEAD")));
    // Untagged path passes.
    assert!(!s.should_skip(Path::new("public/index.html")));
}
