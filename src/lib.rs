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
//!
//! The `prov` reader CLI (`src/bin/prov.rs`, Phase 1 of the PRD) reads
//! this same xattr set back off a path — from either this overlay or
//! the in-kernel LSM under `lsm/`, which stamps a superset of these
//! keys plus an enriched fallback string when no AgentNS session id is
//! present. Its supporting logic lives in:
//!
//! - [`session`]: classify a `user.prov.session` value (AgentNS id vs.
//!   the kernel's enriched fallback vs. this overlay's legacy fallback).
//! - [`duration`]: parse the `24h`/`7d`/`30m` forms `prov find --since` takes.
//! - [`reader`]: read the full xattr set off a path, parse `user.prov.ts`
//!   and `user.prov.history`, and recursively walk a tree for `find`.

pub mod duration;
pub mod fs;
pub mod history;
pub mod identity;
pub mod reader;
pub mod session;
pub mod skip;
pub mod xattrs;

pub use fs::ProvFs;
pub use identity::{Identity, resolve_identity};
pub use skip::SkipList;
