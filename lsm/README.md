# provfs LSM — kernel half

In-tree Linux Security Module that stamps `user.prov.session` and
`user.prov.ts` xattrs on every file successfully closed after being
opened for write. v0.4 adds three more best-effort xattrs read from the
writer's environment — `user.prov.tool`, `user.prov.turn`,
`user.prov.intent` — and a sysctl-tunable skip-prefix list.

Pairs with the FUSE-overlay userspace half in `../src/` for filesystems
that don't support user xattrs natively, and with the separately-shipped
`prov` Rust CLI (userspace).

## Status

v0.4, Phase 1 of [PRD-provenance-fs.md](../../autobuilder/PRDs-archive/PRD-provenance-fs.md):

| Phase | Scope | Status |
|-------|-------|--------|
| 0 | `file_release` hook → stamp `user.prov.session` + `user.prov.ts`. Hardcoded skip-prefix list. | shipped |
| 1 | Read `$CLAUDE_TOOL` / `$CLAUDE_TURN` / `$AGENTNS_INTENT` from `current->mm` via `access_remote_vm()`. Add `.tool`, `.turn`, `.intent`. Sysctl-tunable skip-prefix list. | shipped (v0.4) |
| 2 | History ring (`user.prov.history`). | deferred |
| 3 | AgentNS integration: read `agent_session_id` directly from `current->agent_ns`. | live — verified 2026-08-31 (first non-init agent NS on the box: `agentns-claude` launcher installed; stamp matched the wrapper's 32-hex session id, distinguishing the direct read from the fallback format) |

## Layout

```
lsm/
├── provfs_lsm.c   the LSM: file_release hook, skip filter,
│                  session+ts rendering (incl. enriched fallback)
├── provfs_work.c  bounded deferred-stamp workqueue + sysctl knobs
├── provfs_work.h  shared decls + PROV_* size caps
├── Kconfig        CONFIG_SECURITY_PROVFS
├── Makefile
└── README.md
```

## Build / install

provfs is a **built-in LSM**, not a loadable module — it uses
`DEFINE_LSM()` which places state in the `.lsm_info.init` section
assembled at kernel build time. It ships as part of the
`linux-wintermute` package; see `~/wintermute/wintermute-kernel/`.

To enable: `CONFIG_SECURITY_PROVFS=y` + add `provfs` to the `lsm=`
kernel cmdline (or rely on `LSM_ORDER_MUTABLE` default registration).

## Skip prefixes (sysctl-tunable, v0.4)

```
/proc/  /sys/  /dev/  /run/  /tmp/
/var/run/  /var/cache/  /var/lib/pacman/
/.git/  /node_modules/  /target/  /.cargo/registry/
```

These substrings anywhere in the canonical path suppress stamping —
this is the default list, loaded at boot into
`/proc/sys/kernel/provfs/skip_prefixes`.

The list is runtime-tunable: read/write a comma-separated string of
substrings (max 1023 bytes) at that sysctl, e.g.

```sh
# add a project-local build dir to the default list
echo "/proc/,/sys/,/dev/,/run/,/tmp/,/var/run/,/var/cache/,/var/lib/pacman/,/.git/,/node_modules/,/target/,/.cargo/registry/,/my-scratch/" \
  > /proc/sys/kernel/provfs/skip_prefixes

cat /proc/sys/kernel/provfs/skip_prefixes
```

A concurrent write can't tear a match seen by the `file_release` hook —
the buffer is guarded by an internal rwlock (write path stages the new
value before taking the lock, so it's never held across a
`copy_from_user()`).

## Verify after boot

```sh
# stamped session and ts on a freshly-written file
echo hi > /tmp/x   # skipped — under /tmp
echo hi > ~/hi.txt
getfattr -d ~/hi.txt
# expected (agentns absent — enriched fallback, v0.3):
# user.prov.session="comm-chain:bash;cwd:/home/jsy;pid:12345;uid:1000"
# user.prov.ts="1717123456"

# with a tool env var set in the writer's environment:
CLAUDE_TOOL=/build bash -c 'echo hi > ~/hi.txt'
getfattr -n user.prov.session ~/hi.txt
# user.prov.session="comm-chain:bash;env:CLAUDE_TOOL=/build;cwd:/home/jsy;pid:…;uid:1000"

# inside a real agent namespace (Phase 3 path — needs agentns-claude installed):
agentns-claude --intent test -- sh -c 'echo hi > ~/hi.txt'
getfattr -n user.prov.session ~/hi.txt
# user.prov.session="<32-hex id matching the launcher's session_id>"

# Phase 1 (v0.4) fields — bare env values, one xattr per var, present
# only when the writer's environment actually set it:
CLAUDE_TOOL=/build CLAUDE_TURN=42 AGENTNS_INTENT=test \
  bash -c 'echo hi > ~/hi.txt'
getfattr -d ~/hi.txt
# user.prov.tool="/build"
# user.prov.turn="42"
# user.prov.intent="test"
```

### Phase-1 environment keys (v0.4)

Three xattrs, each a bare env value (no `KEY=` prefix), truncated to 63
bytes. Rendered in the same single pass over the writer's env block
that produces the v0.3 fallback's `env:` field — see
`provfs_scan_env()` / `provfs_scan_writer_env()` in `provfs_lsm.c`. Any
env var absent from the writer's process means the corresponding xattr
is never written at all (best-effort — no field is ever stamped empty):

| xattr | source env var |
|-------|-----------------|
| `user.prov.tool` | `$CLAUDE_TOOL` |
| `user.prov.turn` | `$CLAUDE_TURN` |
| `user.prov.intent` | `$AGENTNS_INTENT` |

### Enriched fallback value format (v0.3, PRD-provfs-comm-richer)

When no AgentNS session id is present, `user.prov.session` is a
`;`-separated, fixed-order field list:

```
comm-chain:<c0>>c1>c2;env:<KEY>=<val>;cwd:<path>;pid:<p>;uid:<u>
```

- `comm-chain` — the writer's `task->comm` walked up `real_parent` for
  up to 3 levels (`tool>parent>gparent`), stopping at `init`/`systemd`/
  `kthreadd`/PID 1. Names the meaningful actor instead of a pipeline's
  innermost `awk`/`sed`/`install` child.
- `env` — the first of `$CLAUDE_TOOL` / `$AGORABUS_SID` /
  `$CLAUDE_SESSION_ID` found in the writer's environ (value capped 48B).
  Omitted when `current->mm` is gone.
- `cwd` — the writer's working directory. Omitted when `current->fs` is
  gone.
- `pid` / `uid` — always present.

Any field may be absent. The whole value is capped at 256 bytes;
truncation drops fields from the right so the outermost-actor signal
(`comm-chain`) is preserved. The AgentNS-present path is unchanged — a
bare 32-hex id.

## License

GPL-2.0-only (kernel code; matches the rest of `security/`).
