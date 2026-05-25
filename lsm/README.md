# provfs LSM — kernel half

In-tree Linux Security Module that stamps `user.prov.session` and
`user.prov.ts` xattrs on every file successfully closed after being
opened for write.

Pairs with the FUSE-overlay userspace half in `../src/` for filesystems
that don't support user xattrs natively.

## Status

Phase 0 of [PRD-provenance-fs.md](../../autobuilder/PRDs-archive/PRD-provenance-fs.md):

| Phase | Scope | Status |
|-------|-------|--------|
| 0 | `file_release` hook → stamp `user.prov.session` + `user.prov.ts`. Hardcoded skip-prefix list. | shipped |
| 1 | Read `$CLAUDE_TOOL` / `$CLAUDE_SESSION` from `current->mm` via `access_remote_vm()`. Add `.tool`, `.turn`, `.intent`. | deferred |
| 2 | History ring (`user.prov.history`). | deferred |
| 3 | AgentNS integration: read `agent_session_id` directly from `current->agent_ns`. | blocked on agentns Phase 3 |

## Layout

```
lsm/
├── provfs_lsm.c   the LSM
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

## Skip prefixes (hardcoded for v0.1)

```
/proc/  /sys/  /dev/  /run/  /tmp/
/var/run/  /var/cache/  /var/lib/pacman/
/.git/  /node_modules/  /target/  /.cargo/registry/
```

These substrings anywhere in the canonical path suppress stamping. A
sysctl-tunable list lands in Phase 1.

## Verify after boot

```sh
# stamped session and ts on a freshly-written file
echo hi > /tmp/x   # skipped — under /tmp
echo hi > ~/hi.txt
getfattr -d ~/hi.txt
# expected:
# user.prov.session="comm:bash:pid:12345:uid:1000"
# user.prov.ts="1717123456"
```

## License

GPL-2.0-only (kernel code; matches the rest of `security/`).
