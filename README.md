# provfs

FUSE-overlay filesystem that stamps `user.prov.*` xattrs at write-time, so
"who wrote this file" is a single `getfattr -d <path>` call rather than a
multi-source join across ctrace, session JSONLs, and `/proc` walks.

Per [PRD-provenance-fs.md][prd] v0.1 (FUSE-overlay slice — the loadable
LSM variant is deferred until a kernel build cycle).

[prd]: https://github.com/j0yen/autobuilder/blob/master/PRD-provenance-fs.md

## What gets stamped

On every write-path FUSE op (`create`, `write`, `setattr`, `mkdir`,
`release` of a dirty fd), provfs reads the calling task's
`/proc/<pid>/environ`, derives an [`Identity`], and writes:

```text
user.prov.session  = $CLAUDE_SESSION or "comm:<name>:pid:<n>"
user.prov.tool     = $CLAUDE_TOOL or comm
user.prov.turn     = $CLAUDE_TURN     (optional)
user.prov.intent   = $CLAUDE_INTENT   (optional)
user.prov.ts       = RFC3339 instant
user.prov.history  = CSV of up to 5 most-recent session ids, MRU first
```

## Skip list

Defaults skip noisy build/VCS/lockfile paths: `.git/`, `node_modules/`,
`target/`, `.cache/`, `.venv/`, `__pycache__/`, etc. Extend with
`--skip private/,secrets/` (layered on top of defaults).

## Usage

```sh
mkdir -p /tmp/src /tmp/mount
echo hi > /tmp/src/note.md

# Mount the overlay (foreground for clarity)
provfs --source /tmp/src --mount /tmp/mount

# In another shell:
echo "Edit" > /tmp/mount/note.md         # writes go through the overlay
getfattr -d /tmp/src/note.md             # see the stamped xattrs

# Unmount when done
fusermount -u /tmp/mount
```

## Build + test

```sh
cargo build
cargo test
```

19 tests pass (16 unit + 3 integration). The integration suite skips
gracefully when the tmp filesystem doesn't support user xattrs.

## Status

- Layered foundation: `identity`, `skip`, `history`, `xattrs` modules
  with full unit coverage.
- Passthrough FUSE impl covers: `lookup`, `getattr`, `read`, `write`,
  `create`, `release`, `setattr`, `mkdir`, `unlink`, `rmdir`, `readdir`,
  `open`.
- Stubs return ENOSYS on: `rename`, `symlink`, `link`, `fsync`,
  `statfs`, `getxattr`, `setxattr`, `listxattr`. Filling these is
  the obvious next slice.
- `fuser::Filesystem::getattr` signature fix-up for fuser 0.14 applied
  (the PRD scaffold initially had a fuser 0.15+ shape).

## License

Dual MIT/Apache-2.0.
