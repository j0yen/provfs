/* SPDX-License-Identifier: GPL-2.0 */
/*
 * provfs deferred stamping — shared declarations.
 *
 * The file_release hook captures the cheap state (resolved + filtered
 * path's dentry, idmap, rendered session string, timestamp) and hands
 * it to a bounded workqueue. The worker performs the journaled xattr
 * writes off the syscall return / task-teardown path.
 *
 * See PRD-provfs-deferred-stamp.md.
 */
#ifndef _PROVFS_WORK_H
#define _PROVFS_WORK_H

#include <linux/types.h>

struct dentry;
struct mnt_idmap;

/* Shared with provfs_lsm.c — sizes of the rendered value strings. */
#define PROV_SESSION_KEY	XATTR_USER_PREFIX "prov.session"
#define PROV_TS_KEY		XATTR_USER_PREFIX "prov.ts"
/*
 * v0.4 (Phase 1): three writer-environment fields, each a bare value
 * (no "KEY=" prefix) read from the writer's env in the same pass that
 * feeds the v0.3 enriched-fallback "env:" field. Best effort — a
 * missing env var means an empty string, and provfs_stamp_worker()
 * skips the xattr write entirely for an empty field (never stamps an
 * empty xattr).
 */
#define PROV_TOOL_KEY		XATTR_USER_PREFIX "prov.tool"	/* $CLAUDE_TOOL */
#define PROV_TURN_KEY		XATTR_USER_PREFIX "prov.turn"	/* $CLAUDE_TURN */
#define PROV_INTENT_KEY		XATTR_USER_PREFIX "prov.intent"	/* $AGENTNS_INTENT */
/*
 * v0.3 (PRD-provfs-comm-richer): the enriched fallback value
 * (comm-chain;env;cwd;pid;uid) is bounded at 256 bytes per §2.2. The
 * agentns hex id and the legacy comm: form are both far shorter, so one
 * cap covers all three. This is the §1.3 hook-time capture buffer: the
 * full enriched string is rendered in the hook (provfs_build_session)
 * and carried through the work payload's session[] field, so the worker
 * never has to re-read the (possibly torn-down) originating task.
 */
#define PROV_IDENT_MAX		256
#define PROV_TS_MAX		24
#define PROV_FIELD_MAX		64	/* v0.4: tool/turn/intent field cap */

/*
 * Allocate the workqueue + register the sysctl table. Called from
 * provfs_init() after security_add_hooks(). Returns 0 on success, a
 * negative errno on workqueue-alloc failure (sysctl failure is
 * non-fatal — logged and ignored).
 */
int provfs_work_init(void);

/*
 * Drain + tear down the workqueue and unregister the sysctl table.
 * LSMs don't unload today; provided for symmetry / future module
 * conversion and so the init failure path can unwind cleanly.
 */
void provfs_work_exit(void);

/*
 * Enqueue a deferred stamp. Takes a reference on @dentry (released by
 * the worker). Never sleeps (GFP_ATOMIC). On queue overflow or alloc
 * failure the request is dropped and the dropped counter bumped —
 * provenance is best-effort.
 *
 * @session and @ts are copied into the work item; the caller's buffers
 * may go out of scope immediately after the call returns.
 *
 * @tool, @turn, @intent (v0.4) are the best-effort Phase-1 fields — any
 * of them may be an empty string (env var absent) or NULL; the worker
 * writes user.prov.{tool,turn,intent} only for the non-empty ones.
 */
void provfs_enqueue_stamp(struct dentry *dentry, struct mnt_idmap *idmap,
			  const char *session, const char *ts,
			  const char *tool, const char *turn,
			  const char *intent);

/*
 * Sysctl-tunable skip-prefix matcher (v0.4,
 * /proc/sys/kernel/provfs/skip_prefixes). @path is skipped (stamping
 * suppressed) if any comma-separated substring in the current list
 * appears anywhere in it. Safe to call from task context on the
 * file_release hot path; internally guards the shared buffer against a
 * concurrent sysctl write so a match is never taken against a torn
 * buffer. Returns true (skip) for a NULL @path.
 */
bool provfs_path_skipped(const char *path);

#endif /* _PROVFS_WORK_H */
