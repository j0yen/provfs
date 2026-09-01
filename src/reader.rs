//! `prov` reader: pull the `user.prov.*` xattr set off a real path,
//! walk a file's stamped history, and recursively search a tree.
//!
//! Unstamped files are the normal case (per PRD §6.1: xattrs are a
//! best-effort hint layer). Every function here returns an absence,
//! never an error, for a missing xattr; a genuine `Err` means a real
//! I/O problem (permission denied, path doesn't exist) that the
//! caller should surface and move past.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::history::MAX_HISTORY;
use crate::xattrs::{KEY_HISTORY, KEY_INTENT, KEY_SESSION, KEY_TOOL, KEY_TS, KEY_TURN};

/// The full set of `user.prov.*` values read off one path. Every field
/// is `None` when the key was never stamped (normal, not an error).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvRecord {
    /// Raw `user.prov.session` value.
    pub session: Option<String>,
    /// Raw `user.prov.tool` value.
    pub tool: Option<String>,
    /// Raw `user.prov.turn` value.
    pub turn: Option<String>,
    /// Raw `user.prov.intent` value.
    pub intent: Option<String>,
    /// Raw `user.prov.ts` value (unix seconds from the kernel LSM, or
    /// an RFC3339 instant from the FUSE overlay — see [`parse_ts`]).
    pub ts: Option<String>,
    /// Raw `user.prov.history` CSV.
    pub history: Option<String>,
}

impl ProvRecord {
    /// True when none of the `user.prov.*` keys were present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.session.is_none()
            && self.tool.is_none()
            && self.turn.is_none()
            && self.intent.is_none()
            && self.ts.is_none()
            && self.history.is_none()
    }
}

/// Read one xattr key, treating "attribute absent" and "xattrs
/// unsupported on this filesystem" both as `Ok(None)`. Any other error
/// (permission denied, path gone) is propagated.
fn get_key(path: &Path, key: &str) -> io::Result<Option<String>> {
    match xattr::get(path, key) {
        Ok(Some(v)) => Ok(Some(String::from_utf8_lossy(&v).into_owned())),
        Ok(None) => Ok(None),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENODATA) | Some(libc::EOPNOTSUPP)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read the full `user.prov.*` set off `path`.
///
/// Returns `Err` only for a genuine I/O problem (permission denied,
/// path doesn't exist) — a path with no provenance at all comes back
/// as `Ok(ProvRecord::default())`.
pub fn read_record(path: &Path) -> io::Result<ProvRecord> {
    // Fail fast (and with a clear error) on a path that doesn't exist,
    // rather than letting the first xattr lookup's ENOENT stand in.
    fs::symlink_metadata(path)?;
    Ok(ProvRecord {
        session: get_key(path, KEY_SESSION)?,
        tool: get_key(path, KEY_TOOL)?,
        turn: get_key(path, KEY_TURN)?,
        intent: get_key(path, KEY_INTENT)?,
        ts: get_key(path, KEY_TS)?,
        history: get_key(path, KEY_HISTORY)?,
    })
}

/// Parsed `user.prov.ts` value: kept as both the raw string and (when
/// parseable) a unix-epoch-seconds integer usable for `--since`
/// comparisons and local-time rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsInfo {
    /// The raw xattr value, unmodified.
    pub raw: String,
    /// Unix seconds, when the raw value parsed as one of the two
    /// formats the writers use (kernel: bare unix seconds; FUSE
    /// overlay: RFC3339).
    pub unix: Option<i64>,
}

/// Parse a `user.prov.ts` value. The kernel LSM stamps bare unix
/// seconds; the FUSE overlay in this repo currently stamps RFC3339 —
/// both are accepted so `prov` works against either backend.
#[must_use]
pub fn parse_ts(raw: &str) -> TsInfo {
    let unix = raw
        .trim()
        .parse::<i64>()
        .ok()
        .or_else(|| chrono::DateTime::parse_from_rfc3339(raw.trim()).ok().map(|dt| dt.timestamp()));
    TsInfo { raw: raw.to_string(), unix }
}

/// Render a unix timestamp as a local datetime string, or `None` if
/// out of `chrono`'s representable range.
#[must_use]
pub fn render_local(unix: i64) -> Option<String> {
    let utc = chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0)?;
    let local: chrono::DateTime<chrono::Local> = utc.with_timezone(&chrono::Local);
    Some(local.format("%Y-%m-%d %H:%M:%S %Z").to_string())
}

/// One entry in a `user.prov.history` ring, in MRU-first order.
#[must_use]
pub fn parse_history(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(MAX_HISTORY)
        .map(str::to_string)
        .collect()
}

/// One filesystem entry visited by [`walk_files`]: its path, and the
/// provenance record read off it (or the error reading it, reported
/// but not fatal to the walk).
pub struct WalkEntry {
    /// Path of the visited file.
    pub path: PathBuf,
    /// The read outcome for this path.
    pub record: io::Result<ProvRecord>,
}

/// Recursively walk `root`, yielding one [`WalkEntry`] per regular
/// file found (symlinks are not followed, to avoid cycles). Directory
/// read errors are reported via a `WalkEntry` whose `record` is `Err`,
/// keyed at the directory path, rather than aborting the walk.
pub fn walk_files(root: &Path) -> Vec<WalkEntry> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                out.push(WalkEntry { path: dir, record: Err(e) });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    out.push(WalkEntry { path: dir.clone(), record: Err(e) });
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(e) => {
                    out.push(WalkEntry { path, record: Err(e) });
                    continue;
                }
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_file() {
                let record = read_record(&path);
                out.push(WalkEntry { path, record });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ts_accepts_unix_seconds() {
        let info = parse_ts("1717123456");
        assert_eq!(info.unix, Some(1_717_123_456));
    }

    #[test]
    fn parse_ts_accepts_rfc3339() {
        let info = parse_ts("2026-05-24T00:00:00Z");
        assert_eq!(info.unix, Some(1_779_580_800));
    }

    #[test]
    fn parse_ts_rejects_garbage() {
        let info = parse_ts("not-a-timestamp");
        assert_eq!(info.unix, None);
        assert_eq!(info.raw, "not-a-timestamp");
    }

    #[test]
    fn render_local_produces_nonempty_string() {
        let s = render_local(1_717_123_456).unwrap();
        assert!(!s.is_empty());
    }

    #[test]
    fn parse_history_splits_and_trims() {
        assert_eq!(parse_history("a, b ,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_history_caps_at_max() {
        let raw = "a,b,c,d,e,f,g";
        assert_eq!(parse_history(raw).len(), MAX_HISTORY);
    }

    #[test]
    fn empty_record_reports_is_empty() {
        assert!(ProvRecord::default().is_empty());
    }

    #[test]
    fn nonempty_record_reports_not_empty() {
        let r = ProvRecord {
            tool: Some("Edit".to_string()),
            ..Default::default()
        };
        assert!(!r.is_empty());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod integration_tests {
    use super::*;
    use tempfile::TempDir;

    fn xattr_supported(path: &Path) -> bool {
        let probe = "user.provfs_reader.probe";
        match xattr::set(path, probe, b"1") {
            Ok(()) => {
                let _ = xattr::remove(path, probe);
                true
            }
            Err(_) => false,
        }
    }

    #[test]
    fn read_record_round_trips_real_xattrs() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("note.txt");
        fs::write(&file, b"hi").unwrap();
        if !xattr_supported(&file) {
            eprintln!("skip: xattrs unsupported on this tmp filesystem");
            return;
        }
        xattr::set(&file, KEY_SESSION, b"01234567890abcdef01234567890abc").unwrap();
        xattr::set(&file, KEY_TOOL, b"Edit").unwrap();
        xattr::set(&file, KEY_TS, b"1717123456").unwrap();

        let rec = read_record(&file).unwrap();
        assert_eq!(rec.session.as_deref(), Some("01234567890abcdef01234567890abc"));
        assert_eq!(rec.tool.as_deref(), Some("Edit"));
        assert_eq!(rec.ts.as_deref(), Some("1717123456"));
        assert_eq!(rec.turn, None);
        assert!(!rec.is_empty());
    }

    #[test]
    fn read_record_on_unstamped_file_is_empty_not_error() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plain.txt");
        fs::write(&file, b"hi").unwrap();
        let rec = read_record(&file).unwrap();
        assert!(rec.is_empty());
    }

    #[test]
    fn read_record_on_missing_path_errors() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist.txt");
        assert!(read_record(&missing).is_err());
    }

    #[test]
    fn walk_files_finds_stamped_file_recursively() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let file = sub.join("note.txt");
        fs::write(&file, b"hi").unwrap();
        if xattr_supported(&file) {
            xattr::set(&file, KEY_TOOL, b"Edit").unwrap();
        }

        let entries = walk_files(dir.path());
        let found = entries.iter().find(|e| e.path == file);
        assert!(found.is_some(), "expected to find {file:?} in walk");
    }
}
