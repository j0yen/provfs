//! provfs — FUSE-overlay filesystem that stamps `user.prov.*` xattrs at write-time.
//!
//! Per PRD-provenance-fs.md v0.1 (FUSE-overlay slice — the loadable LSM
//! variant is deferred until a kernel build cycle).
//!
//! Layered as:
//!
//! - [`identity`]: resolve the calling task's session/tool/turn/intent by
//!   reading `/proc/<pid>/environ` of the FUSE request peer. Falls back to
//!   `comm:<name>:pid:<n>` when env vars are absent.
//! - [`skip`]: prefix-based exclusion list (`.git/objects`, `node_modules`,
//!   `target/`, etc.).
//! - [`history`]: MRU ring of the last N sessions that touched the file,
//!   serialised as a CSV in `user.prov.history`.
//! - [`xattrs`]: low-level xattr write helpers wrapping the `xattr` crate.
//! - [`fs`]: the [`ProvFs`] passthrough filesystem itself.
//!
//! All write paths stamp these xattrs on the underlying inode:
//!
//! ```text
//! user.prov.session  = "01KS…" or "comm:<name>:pid:<n>"
//! user.prov.tool     = "$CLAUDE_TOOL" or comm
//! user.prov.turn     = "$CLAUDE_TURN"   (optional)
//! user.prov.intent   = "$CLAUDE_INTENT" (optional)
//! user.prov.ts       = RFC3339 instant
//! user.prov.history  = CSV of up to 5 most-recent session ids, MRU first
//! ```

pub mod fs;
pub mod history;
pub mod identity;
pub mod skip;
pub mod xattrs;

pub use fs::ProvFs;
pub use identity::{Identity, resolve_identity};
pub use skip::SkipList;
