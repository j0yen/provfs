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
 */
void provfs_enqueue_stamp(struct dentry *dentry, struct mnt_idmap *idmap,
			  const char *session, const char *ts);

#endif /* _PROVFS_WORK_H */
