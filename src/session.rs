//! Classify a `user.prov.session` value into one of the shapes the
//! kernel LSM or the FUSE overlay can stamp:
//!
//! - a bare 32-hex AgentNS session id
//! - the kernel's enriched fallback (`comm-chain:...;env:...;cwd:...;pid:...;uid:...`)
//! - the FUSE overlay's legacy fallback (`comm:<name>:pid:<n>`)
//! - anything else, printed opaque/raw
//!
//! Per `lsm/README.md` ("Enriched fallback value format, v0.3"), fields
//! in the enriched form may be absent (truncated from the right), so
//! parsing is field-by-field rather than positional.

/// The parsed fields of a kernel enriched fallback session string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FallbackFields {
    /// `comm-chain` — writer comm walked up to ~3 `real_parent` levels, `>`-joined.
    pub comm_chain: Option<String>,
    /// `env` — first of `$CLAUDE_TOOL` / `$AGORABUS_SID` / `$CLAUDE_SESSION_ID` found.
    pub env: Option<String>,
    /// `cwd` — writer's working directory at stamp time.
    pub cwd: Option<String>,
    /// `pid` — writer's pid.
    pub pid: Option<String>,
    /// `uid` — writer's uid.
    pub uid: Option<String>,
}

/// Classification of a `user.prov.session` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionKind {
    /// A bare 32-hex AgentNS session id, unmodified.
    AgentNs(String),
    /// The kernel LSM's enriched fallback (`comm-chain:...;...`).
    EnrichedFallback(FallbackFields),
    /// The FUSE overlay's legacy fallback (`comm:<name>:pid:<n>`).
    LegacyFallback {
        /// Process comm name.
        comm: String,
        /// Process pid.
        pid: String,
    },
    /// Anything else — printed as-is.
    Opaque(String),
}

fn is_agentns_id(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_enriched_fallback(s: &str) -> FallbackFields {
    let mut f = FallbackFields::default();
    for segment in s.split(';') {
        let Some((key, value)) = segment.split_once(':') else {
            continue;
        };
        match key {
            "comm-chain" => f.comm_chain = Some(value.to_string()),
            "env" => f.env = Some(value.to_string()),
            "cwd" => f.cwd = Some(value.to_string()),
            "pid" => f.pid = Some(value.to_string()),
            "uid" => f.uid = Some(value.to_string()),
            _ => {}
        }
    }
    f
}

fn parse_legacy_fallback(s: &str) -> Option<(String, String)> {
    // "comm:<name>:pid:<n>"
    let rest = s.strip_prefix("comm:")?;
    let (name, tail) = rest.split_once(":pid:")?;
    if name.is_empty() || tail.is_empty() {
        return None;
    }
    Some((name.to_string(), tail.to_string()))
}

/// Classify a raw `user.prov.session` value.
#[must_use]
pub fn classify(session: &str) -> SessionKind {
    if is_agentns_id(session) {
        return SessionKind::AgentNs(session.to_string());
    }
    if session.starts_with("comm-chain:") {
        return SessionKind::EnrichedFallback(parse_enriched_fallback(session));
    }
    if let Some((comm, pid)) = parse_legacy_fallback(session) {
        return SessionKind::LegacyFallback { comm, pid };
    }
    SessionKind::Opaque(session.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_agentns_id() {
        let id = "01234567890abcdef01234567890abc";
        assert_eq!(classify(id), SessionKind::AgentNs(id.to_string()));
    }

    #[test]
    fn agentns_id_is_case_insensitive_hex() {
        let id = "ABCDEF0123456789ABCDEF0123456789";
        assert!(matches!(classify(id), SessionKind::AgentNs(_)));
    }

    #[test]
    fn rejects_wrong_length_as_not_agentns() {
        // 31 hex chars -> not an agentns id, falls through to opaque.
        let s = "0123456789abcdef0123456789abcde";
        assert_eq!(s.len(), 31);
        assert!(matches!(classify(s), SessionKind::Opaque(_)));
    }

    #[test]
    fn classifies_enriched_fallback_full() {
        let s = "comm-chain:bash>login>init;env:CLAUDE_TOOL=/build;cwd:/home/jsy;pid:12345;uid:1000";
        let SessionKind::EnrichedFallback(f) = classify(s) else {
            panic!("expected enriched fallback");
        };
        assert_eq!(f.comm_chain.as_deref(), Some("bash>login>init"));
        assert_eq!(f.env.as_deref(), Some("CLAUDE_TOOL=/build"));
        assert_eq!(f.cwd.as_deref(), Some("/home/jsy"));
        assert_eq!(f.pid.as_deref(), Some("12345"));
        assert_eq!(f.uid.as_deref(), Some("1000"));
    }

    #[test]
    fn classifies_enriched_fallback_truncated() {
        // Truncation drops fields from the right per lsm/README.md.
        let s = "comm-chain:bash;env:CLAUDE_TOOL=/build";
        let SessionKind::EnrichedFallback(f) = classify(s) else {
            panic!("expected enriched fallback");
        };
        assert_eq!(f.comm_chain.as_deref(), Some("bash"));
        assert_eq!(f.env.as_deref(), Some("CLAUDE_TOOL=/build"));
        assert_eq!(f.cwd, None);
        assert_eq!(f.pid, None);
        assert_eq!(f.uid, None);
    }

    #[test]
    fn classifies_legacy_fallback() {
        let s = "comm:claude:pid:1202";
        assert_eq!(
            classify(s),
            SessionKind::LegacyFallback {
                comm: "claude".to_string(),
                pid: "1202".to_string(),
            }
        );
    }

    #[test]
    fn classifies_opaque_for_unknown_shape() {
        let s = "some-random-string";
        assert_eq!(classify(s), SessionKind::Opaque(s.to_string()));
    }
}
