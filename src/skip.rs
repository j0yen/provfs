//! Prefix-based skip list for paths that should NOT be stamped.
//!
//! Defaults capture noisy build / VCS / lockfile traffic that would
//! otherwise dominate xattr writes.

use std::path::Path;

/// Default skip prefixes (path relative to the FUSE mount source root).
pub const DEFAULT_SKIP_PREFIXES: &[&str] = &[
    ".git/",
    ".git/objects/",
    "node_modules/",
    "target/",
    ".cache/",
    ".venv/",
    "venv/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    "dist/",
    "build/",
];

/// Prefix matcher. Cheap linear scan; the list is short.
#[derive(Debug, Clone)]
pub struct SkipList {
    prefixes: Vec<String>,
}

impl SkipList {
    /// Build from the embedded defaults.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            prefixes: DEFAULT_SKIP_PREFIXES.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// Build from a comma-separated user spec, layered on top of the defaults.
    #[must_use]
    pub fn from_user_spec(extra: &str) -> Self {
        let mut s = Self::defaults();
        for raw in extra.split(',') {
            let p = raw.trim();
            if !p.is_empty() {
                s.prefixes.push(p.to_string());
            }
        }
        s
    }

    /// Empty skip list (for tests).
    #[must_use]
    pub fn empty() -> Self {
        Self { prefixes: Vec::new() }
    }

    /// Returns true if the given relative path (already stripped of
    /// the FUSE mount root) starts with any skip prefix or contains
    /// a `/<prefix>` segment.
    #[must_use]
    pub fn should_skip(&self, rel: &Path) -> bool {
        let s = rel.to_string_lossy();
        for prefix in &self.prefixes {
            if s.starts_with(prefix.as_str()) {
                return true;
            }
            let needle = format!("/{prefix}");
            if s.contains(needle.as_str()) {
                return true;
            }
        }
        false
    }
}

impl Default for SkipList {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_skip_git_objects() {
        let s = SkipList::defaults();
        assert!(s.should_skip(Path::new(".git/objects/abcd")));
        assert!(s.should_skip(Path::new("repo/.git/HEAD")));
        assert!(s.should_skip(Path::new("frontend/node_modules/react/package.json")));
        assert!(s.should_skip(Path::new("rust-crate/target/debug/foo")));
    }

    #[test]
    fn defaults_pass_through_normal_paths() {
        let s = SkipList::defaults();
        assert!(!s.should_skip(Path::new("notes.md")));
        assert!(!s.should_skip(Path::new("src/main.rs")));
        assert!(!s.should_skip(Path::new("Cargo.toml")));
    }

    #[test]
    fn user_spec_layered_on_defaults() {
        let s = SkipList::from_user_spec("logs/,tmp/");
        assert!(s.should_skip(Path::new("logs/build.log")));
        assert!(s.should_skip(Path::new("tmp/foo")));
        assert!(s.should_skip(Path::new(".git/HEAD")));
        assert!(!s.should_skip(Path::new("README.md")));
    }

    #[test]
    fn empty_list_skips_nothing() {
        let s = SkipList::empty();
        assert!(!s.should_skip(Path::new(".git/HEAD")));
    }
}
