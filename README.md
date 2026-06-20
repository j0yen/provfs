# provfs

A filesystem that records who wrote each file at the moment of writing, so "where did this come from?" is one `getfattr` call instead of a forensic reconstruction.

Provenance is cheap to capture and expensive to recover. At write-time the kernel knows exactly which process, session, and tool produced a byte; an hour later, answering the same question means joining ctrace output against session JSONLs against `/proc` walks, and guessing where they disagree. provfs captures the answer when it's free. Every write gets stamped with `user.prov.*` xattrs — session, tool, timestamp, and a short history of recent sessions — and from then on the file carries its own origin.

provfs has two halves, and they solve the same problem at different layers:

- **A FUSE overlay** (Rust, `src/`) — mount it over a directory and writes through the mount get stamped. Runs in userspace, needs no special kernel, works today.
- **A built-in LSM** (C, `lsm/`) — the same stamping done in-kernel via the `file_release` hook, for when you want it on every write to a real filesystem rather than only through an overlay. Phase 0 is shipped; it builds into `linux-wintermute`.

The overlay is the part you can run on a stock machine. The LSM is the part that needs a kernel build.

## What gets stamped

On each write-path operation (`create`, `write`, `setattr`, `mkdir`, and `release` of a dirty fd), the overlay reads the calling task's environment, derives an identity, and writes:

```text
user.prov.session  = $CLAUDE_SESSION, or "comm:<name>:pid:<n>" when absent
user.prov.tool     = $CLAUDE_TOOL, or the process comm
user.prov.turn     = $CLAUDE_TURN     (optional)
user.prov.intent   = $CLAUDE_INTENT   (optional)
user.prov.ts       = RFC3339 instant
user.prov.history  = CSV of up to 5 most-recent session ids, most-recent first
```

## Run the overlay

The overlay is the runnable half — build it with cargo and mount it.

```sh
cargo build --release

mkdir -p /tmp/src /tmp/mount
echo hi > /tmp/src/note.md

# Mount the overlay over the source dir (foreground).
provfs --source /tmp/src --mount /tmp/mount

# In another shell — writes through the mount get stamped on the backing file:
echo "Edit" > /tmp/mount/note.md
getfattr -d /tmp/src/note.md

# Done:
fusermount -u /tmp/mount
```

Flags: `--source` (backing dir), `--mount` (mountpoint), `--skip` (extra comma-separated skip prefixes, layered on top of the defaults), `--foreground`.

### Skip list

By default provfs skips the paths that generate write noise without provenance value: `.git/`, `node_modules/`, `target/`, `.cache/`, `.venv/`, `__pycache__/`, and similar. `--skip private/,secrets/` adds to that set rather than replacing it.

## Build and test

```sh
cargo build
cargo test
```

19 tests — 16 unit across the `identity`, `skip`, `history`, and `xattrs` modules, plus 3 integration. The integration suite skips cleanly when the temp filesystem doesn't support user xattrs, so a machine without xattr support reports honestly rather than failing.

## How it's built

The overlay is a passthrough FUSE filesystem with stamping spliced into the write path. Implemented ops: `lookup`, `getattr`, `read`, `write`, `create`, `release`, `setattr`, `mkdir`, `unlink`, `rmdir`, `readdir`, `open`. The rest — `rename`, `symlink`, `link`, `fsync`, `statfs`, and the xattr ops — return `ENOSYS` for now; filling them is the next slice. Identity, skip-filtering, history, and xattr rendering each live in their own module under full unit coverage, so the FUSE layer stays a thin shell over tested logic.

The kernel half is documented in [`lsm/README.md`](lsm/README.md): a built-in (not loadable) LSM that stamps `user.prov.session` and `user.prov.ts` on file release, with a comm-chain-plus-environ enriched fallback when no agent session id is present. It's GPL-2.0, matching the rest of `security/`.

## Status

The overlay runs today. The LSM is Phase 0 — session and timestamp stamping on `file_release`, with a hardcoded skip-prefix list; the tool/turn/intent keys and a sysctl-tunable skip list are later phases. See `lsm/README.md` for the phase table.

## Where it fits

provfs is the capture layer for wintermute provenance. [provenance-mcp](https://github.com/j0yen/provenance-mcp) is the read side — it exposes these same `user.prov.*` xattrs to an agent over MCP. Part of the [wintermute](https://github.com/j0yen/wintermute) line of work.

## License

The Rust overlay is MIT OR Apache-2.0. The LSM kernel code under `lsm/` is GPL-2.0-only.
