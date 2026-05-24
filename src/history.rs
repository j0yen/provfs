//! MRU ring for `user.prov.history`: CSV of up to N session ids, MRU first.
//!
//! Per PRD §4.2, N=5. A larger history belongs in `fsstory`'s external
//! store; the xattr only carries the recent tip.

/// Maximum number of session ids kept in the ring.
pub const MAX_HISTORY: usize = 5;

/// Push `session` onto the front of `prior` (a CSV-encoded history),
/// dropping duplicates and truncating to [`MAX_HISTORY`]. Returns the
/// new CSV.
#[must_use]
pub fn push_history(prior: &str, session: &str) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(MAX_HISTORY);
    out.push(session);
    for s in prior.split(',') {
        let s = s.trim();
        if s.is_empty() || s == session {
            continue;
        }
        if out.len() == MAX_HISTORY {
            break;
        }
        out.push(s);
    }
    out.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_push_into_empty() {
        assert_eq!(push_history("", "a"), "a");
    }

    #[test]
    fn dedup_when_pushing_existing_head() {
        // Same session re-touching the file shouldn't grow the list.
        assert_eq!(push_history("a,b,c", "a"), "a,b,c");
    }

    #[test]
    fn dedup_when_pushing_existing_tail() {
        assert_eq!(push_history("a,b,c", "c"), "c,a,b");
    }

    #[test]
    fn truncates_at_max() {
        let prior = "a,b,c,d,e";
        let got = push_history(prior, "f");
        assert_eq!(got, "f,a,b,c,d");
        assert_eq!(got.split(',').count(), MAX_HISTORY);
    }

    #[test]
    fn ignores_whitespace_and_empty_segments() {
        assert_eq!(push_history(" a , , b ", "c"), "c,a,b");
    }
}
