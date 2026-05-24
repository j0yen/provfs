//! Xattr write helpers. Thin wrapper around the `xattr` crate that
//! folds an [`Identity`] into the canonical `user.prov.*` key set.

use std::io;
use std::path::Path;

use crate::history::push_history;
use crate::identity::Identity;

/// Canonical xattr key names.
pub const KEY_SESSION: &str = "user.prov.session";
pub const KEY_TOOL: &str = "user.prov.tool";
pub const KEY_TURN: &str = "user.prov.turn";
pub const KEY_INTENT: &str = "user.prov.intent";
pub const KEY_TS: &str = "user.prov.ts";
pub const KEY_HISTORY: &str = "user.prov.history";

fn read_str(path: &Path, key: &str) -> Option<String> {
    let v = xattr::get(path, key).ok().flatten()?;
    String::from_utf8(v).ok()
}

fn write_str(path: &Path, key: &str, value: &str) -> io::Result<()> {
    xattr::set(path, key, value.as_bytes())
}

/// Stamp the full `user.prov.*` set on `path` for `id`, advancing the
/// history ring.
///
/// `now_iso` is provided by the caller so tests are deterministic.
pub fn stamp(path: &Path, id: &Identity, now_iso: &str) -> io::Result<()> {
    write_str(path, KEY_SESSION, &id.session)?;
    write_str(path, KEY_TOOL, &id.tool)?;
    if let Some(t) = &id.turn {
        write_str(path, KEY_TURN, t)?;
    }
    if let Some(i) = &id.intent {
        write_str(path, KEY_INTENT, i)?;
    }
    write_str(path, KEY_TS, now_iso)?;

    let prior = read_str(path, KEY_HISTORY).unwrap_or_default();
    let next = push_history(&prior, &id.session);
    write_str(path, KEY_HISTORY, &next)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    fn xattr_supported(path: &Path) -> bool {
        // Best-effort probe: try a tiny set/get/remove cycle.
        let probe = "user.provfs.probe";
        match xattr::set(path, probe, b"1") {
            Ok(()) => {
                let _ = xattr::remove(path, probe);
                true
            }
            Err(_) => false,
        }
    }

    #[test]
    fn stamp_writes_canonical_keys() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        File::create(&p).unwrap();
        if !xattr_supported(&p) {
            eprintln!("skip: xattr unsupported on tmpfs/working fs");
            return;
        }
        let id = Identity {
            session: "S1".into(),
            tool: "Edit".into(),
            turn: Some("3".into()),
            intent: Some("self-review".into()),
        };
        stamp(&p, &id, "2026-05-24T00:00:00Z").unwrap();

        assert_eq!(read_str(&p, KEY_SESSION).as_deref(), Some("S1"));
        assert_eq!(read_str(&p, KEY_TOOL).as_deref(), Some("Edit"));
        assert_eq!(read_str(&p, KEY_TURN).as_deref(), Some("3"));
        assert_eq!(read_str(&p, KEY_INTENT).as_deref(), Some("self-review"));
        assert_eq!(read_str(&p, KEY_TS).as_deref(), Some("2026-05-24T00:00:00Z"));
        assert_eq!(read_str(&p, KEY_HISTORY).as_deref(), Some("S1"));
    }

    #[test]
    fn history_grows_mru_first() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("b.txt");
        File::create(&p).unwrap();
        if !xattr_supported(&p) {
            return;
        }
        let s1 = Identity {
            session: "S1".into(),
            tool: "t".into(),
            ..Default::default()
        };
        let s2 = Identity {
            session: "S2".into(),
            tool: "t".into(),
            ..Default::default()
        };
        stamp(&p, &s1, "t").unwrap();
        stamp(&p, &s2, "t").unwrap();
        assert_eq!(read_str(&p, KEY_HISTORY).as_deref(), Some("S2,S1"));
    }
}
