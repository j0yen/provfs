//! Resolve the calling task's identity from its `/proc/<pid>/environ`.
//!
//! FUSE delivers the requester's pid via [`fuser::Request::pid`]. We open
//! the corresponding `/proc/<pid>/environ` file and look up the
//! `CLAUDE_TOOL`, `CLAUDE_TURN`, `CLAUDE_SESSION`, and `CLAUDE_INTENT`
//! env vars. If any are missing we fall back to `comm` (from
//! `/proc/<pid>/comm`).

use std::fs;
use std::path::PathBuf;

/// Identity of the task that issued the current FUSE request.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    /// Stable session id (ULID-shaped) when available, else `comm:<name>:pid:<n>`.
    pub session: String,
    /// Tool name (Edit, Write, bash, cargo, …) or comm.
    pub tool: String,
    /// Optional Claude conversation turn number.
    pub turn: Option<String>,
    /// Optional intent tag (e.g. "self-review", "autobuilder").
    pub intent: Option<String>,
}

/// Read `key=value` pairs from `/proc/<pid>/environ` (NUL-separated).
fn read_environ(pid: u32) -> Vec<(String, String)> {
    let path = PathBuf::from(format!("/proc/{pid}/environ"));
    let Ok(bytes) = fs::read(&path) else {
        return Vec::new();
    };
    bytes
        .split(|b| *b == 0)
        .filter_map(|chunk| {
            if chunk.is_empty() {
                return None;
            }
            let s = std::str::from_utf8(chunk).ok()?;
            let (k, v) = s.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

fn read_comm(pid: u32) -> Option<String> {
    let path = PathBuf::from(format!("/proc/{pid}/comm"));
    fs::read_to_string(&path).ok().map(|s| s.trim().to_string())
}

/// Resolve an [`Identity`] for the given pid.
#[must_use]
pub fn resolve_identity(pid: u32) -> Identity {
    let env = read_environ(pid);
    let comm = read_comm(pid).unwrap_or_else(|| "unknown".to_string());

    let mut session = None;
    let mut tool = None;
    let mut turn = None;
    let mut intent = None;

    for (k, v) in env {
        match k.as_str() {
            "CLAUDE_SESSION" => session = Some(v),
            "CLAUDE_TOOL" => tool = Some(v),
            "CLAUDE_TURN" => turn = Some(v),
            "CLAUDE_INTENT" => intent = Some(v),
            _ => {}
        }
    }

    Identity {
        session: session.unwrap_or_else(|| format!("comm:{comm}:pid:{pid}")),
        tool: tool.unwrap_or(comm),
        turn,
        intent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_falls_back_to_comm_when_no_env() {
        // PID 1 (init/systemd) is unlikely to have CLAUDE_* env vars set.
        let id = resolve_identity(1);
        assert!(
            id.session.starts_with("comm:"),
            "expected comm fallback, got session={}",
            id.session
        );
        assert!(!id.tool.is_empty());
    }

    #[test]
    fn resolve_nonexistent_pid_yields_unknown() {
        // PIDs in this range almost never exist on a normal Linux box.
        let id = resolve_identity(4_000_000);
        assert!(id.session.starts_with("comm:unknown:"));
    }
}
