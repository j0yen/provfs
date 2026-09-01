// SPDX-License-Identifier: GPL-2.0
/*
 * provfs deferred stamping — bounded workqueue + worker + sysctl.
 *
 * v0.1 ran the xattr writes synchronously inside the file_release LSM
 * hook, i.e. on every writable close, including from exit_files()
 * during task teardown. That put two journaled __vfs_setxattr_noperm
 * calls plus a PATH_MAX path walk on a hot path that should be a few
 * instructions, and forced the hook to run defensively against a
 * half-torn-down task_struct.
 *
 * This file moves the real work to a single unbound workqueue. The
 * hook resolves+filters the path, renders session+ts, takes a dget()
 * reference on the dentry, and enqueues a provfs_stamp_work. The worker
 * runs on a kthread with no relationship to the original task, writes
 * the two xattrs, dput()s the dentry, and frees the work. On overflow
 * (queue depth >= queue_max) or alloc failure we drop and bump a
 * counter — provenance is best-effort.
 *
 * v0.4 (Phase 1) adds two independent things to this file:
 *
 *  - Three more best-effort payload fields (tool/turn/intent, read from
 *    the writer's env by the hook — see provfs_lsm.c's env scanner) are
 *    carried through the work item the same way session/ts already
 *    were, and stamped as user.prov.{tool,turn,intent} — but only when
 *    non-empty, so a missing env var never produces an empty xattr.
 *
 *  - The v0.1-v0.3 hardcoded skip-prefix array is replaced by a
 *    runtime-tunable, comma-separated list at
 *    /proc/sys/kernel/provfs/skip_prefixes, guarded by a rwlock
 *    (provfs_skip_lock) since the matcher runs in task context on the
 *    file_release path and must not race a concurrent sysctl write.
 *
 * See PRD-provfs-deferred-stamp.md, PRD-provenance-fs.md §4.1 (Phase 1).
 */

#include <linux/atomic.h>
#include <linux/cred.h>	/* current_fsuid()/current_fsgid() via mnt_idmapping.h */
#include <linux/dcache.h>
#include <linux/init.h>
#include <linux/kernel.h>
#include <linux/mnt_idmapping.h>
#include <linux/slab.h>
#include <linux/spinlock.h>	/* rwlock_t for the skip_prefixes buffer */
#include <linux/string.h>
#include <linux/sysctl.h>
#include <linux/workqueue.h>
#include <linux/xattr.h>

#include "provfs_work.h"

struct provfs_stamp_work {
	struct work_struct	work;
	struct dentry		*dentry;	/* dget() in hook, dput() in worker */
	struct mnt_idmap	*idmap;
	char			session[PROV_IDENT_MAX];
	char			ts[PROV_TS_MAX];
	char			tool[PROV_FIELD_MAX];	/* v0.4: $CLAUDE_TOOL */
	char			turn[PROV_FIELD_MAX];	/* v0.4: $CLAUDE_TURN */
	char			intent[PROV_FIELD_MAX];	/* v0.4: $AGENTNS_INTENT */
};

static struct workqueue_struct *provfs_wq;
static struct ctl_table_header *provfs_sysctl_hdr;

static atomic_t provfs_queue_depth = ATOMIC_INIT(0);
static atomic64_t provfs_queue_dropped = ATOMIC64_INIT(0);

/*
 * queue_max is the only writable knob. queue_depth and queue_dropped
 * are exported read-only via dedicated proc_handlers below so we can
 * snapshot the atomics into a stable temporary for the read.
 */
static int provfs_queue_max = 1024;
static int provfs_queue_max_min = 1;
static int provfs_queue_max_max = 1 << 20;

/*
 * v0.4: sysctl-tunable skip-prefix list, replacing the v0.1-v0.3
 * hardcoded array. Comma-separated substrings; a path is skipped
 * (stamping suppressed) if any of them appears anywhere in it — same
 * matcher semantics as the old array, just runtime-writable at
 * /proc/sys/kernel/provfs/skip_prefixes.
 *
 * provfs_skip_lock guards the buffer against a torn read: the hook
 * runs in task context on file_release (no atomic requirement), so a
 * plain rwlock is enough — readers (the hook, and a sysctl read) take
 * a brief read_lock() just to snapshot the buffer into a local copy;
 * a sysctl write stages the new value in a local buffer first (so the
 * lock is never held across copy_from_user(), which can sleep) and
 * then takes write_lock() only for the final in-kernel copy.
 */
#define PROVFS_SKIP_MAX		1024
#define PROVFS_SKIP_DEFAULT \
	"/proc/,/sys/,/dev/,/run/,/tmp/,/var/run/,/var/cache/," \
	"/var/lib/pacman/,/.git/,/node_modules/,/target/,/.cargo/registry/"

static char provfs_skip_prefixes_buf[PROVFS_SKIP_MAX] = PROVFS_SKIP_DEFAULT;
static DEFINE_RWLOCK(provfs_skip_lock);

bool provfs_path_skipped(const char *path)
{
	char local[PROVFS_SKIP_MAX];
	char *cursor, *tok;

	if (!path)
		return true;

	read_lock(&provfs_skip_lock);
	strscpy(local, provfs_skip_prefixes_buf, sizeof(local));
	read_unlock(&provfs_skip_lock);

	cursor = local;
	while ((tok = strsep(&cursor, ",")) != NULL) {
		if (*tok && strstr(path, tok))
			return true;
	}
	return false;
}

static int provfs_sysctl_skip_prefixes(const struct ctl_table *table,
					int write, void *buffer, size_t *lenp,
					loff_t *ppos)
{
	struct ctl_table t = *table;
	char staging[PROVFS_SKIP_MAX];
	int ret;

	/* Snapshot the current value under the lock first either way. */
	read_lock(&provfs_skip_lock);
	strscpy(staging, provfs_skip_prefixes_buf, sizeof(staging));
	read_unlock(&provfs_skip_lock);

	/*
	 * Hand proc_dostring() the staging copy, never the shared buffer
	 * directly — it may copy_from_user()/copy_to_user(), which can
	 * sleep, and provfs_skip_lock is a non-sleeping rwlock.
	 */
	t.data = staging;
	ret = proc_dostring(&t, write, buffer, lenp, ppos);
	if (ret || !write)
		return ret;

	write_lock(&provfs_skip_lock);
	strscpy(provfs_skip_prefixes_buf, staging, sizeof(provfs_skip_prefixes_buf));
	write_unlock(&provfs_skip_lock);
	return 0;
}

static int provfs_sysctl_depth(const struct ctl_table *table, int write,
			       void *buffer, size_t *lenp, loff_t *ppos)
{
	int snapshot = atomic_read(&provfs_queue_depth);
	struct ctl_table t = *table;

	if (write)
		return -EPERM;
	t.data = &snapshot;
	return proc_dointvec(&t, write, buffer, lenp, ppos);
}

static int provfs_sysctl_dropped(const struct ctl_table *table, int write,
				 void *buffer, size_t *lenp, loff_t *ppos)
{
	long long snapshot = atomic64_read(&provfs_queue_dropped);
	struct ctl_table t = *table;

	if (write)
		return -EPERM;
	t.data = &snapshot;
	return proc_doulongvec_minmax(&t, write, buffer, lenp, ppos);
}

/*
 * Post-6.11 sysctl convention: NO trailing sentinel {} entry. Adding
 * one trips the table-length check and fails registration (this is the
 * exact bug that broke memlog; cf. 2026-05-26 fix).
 */
static struct ctl_table provfs_sysctl_table[] = {
	{
		.procname	= "queue_max",
		.data		= &provfs_queue_max,
		.maxlen		= sizeof(int),
		.mode		= 0644,
		.proc_handler	= proc_dointvec_minmax,
		.extra1		= &provfs_queue_max_min,
		.extra2		= &provfs_queue_max_max,
	},
	{
		.procname	= "queue_depth",
		.maxlen		= sizeof(int),
		.mode		= 0444,
		.proc_handler	= provfs_sysctl_depth,
	},
	{
		.procname	= "queue_dropped",
		.maxlen		= sizeof(long long),
		.mode		= 0444,
		.proc_handler	= provfs_sysctl_dropped,
	},
	{
		.procname	= "skip_prefixes",
		.data		= provfs_skip_prefixes_buf,
		.maxlen		= sizeof(provfs_skip_prefixes_buf),
		.mode		= 0644,
		.proc_handler	= provfs_sysctl_skip_prefixes,
	},
};

static void provfs_stamp_worker(struct work_struct *work)
{
	struct provfs_stamp_work *w =
		container_of(work, struct provfs_stamp_work, work);

	/*
	 * Writing to an inode that was unlinked between enqueue and now
	 * returns -ENOENT; that's expected and intentionally ignored —
	 * provenance is best-effort.
	 */
	(void)__vfs_setxattr_noperm(w->idmap, w->dentry, PROV_SESSION_KEY,
				    w->session, strlen(w->session), 0);
	(void)__vfs_setxattr_noperm(w->idmap, w->dentry, PROV_TS_KEY,
				    w->ts, strlen(w->ts), 0);

	/*
	 * v0.4: best-effort Phase-1 fields. Each is written only when
	 * non-empty — a missing env var means no xattr at all, never an
	 * empty one.
	 */
	if (w->tool[0])
		(void)__vfs_setxattr_noperm(w->idmap, w->dentry, PROV_TOOL_KEY,
					    w->tool, strlen(w->tool), 0);
	if (w->turn[0])
		(void)__vfs_setxattr_noperm(w->idmap, w->dentry, PROV_TURN_KEY,
					    w->turn, strlen(w->turn), 0);
	if (w->intent[0])
		(void)__vfs_setxattr_noperm(w->idmap, w->dentry, PROV_INTENT_KEY,
					    w->intent, strlen(w->intent), 0);

	dput(w->dentry);
	atomic_dec(&provfs_queue_depth);
	kfree(w);
}

void provfs_enqueue_stamp(struct dentry *dentry, struct mnt_idmap *idmap,
			  const char *session, const char *ts,
			  const char *tool, const char *turn,
			  const char *intent)
{
	struct provfs_stamp_work *w;

	if (!provfs_wq || !dentry || !session || !ts)
		goto drop;

	if (atomic_read(&provfs_queue_depth) >= READ_ONCE(provfs_queue_max))
		goto drop;

	/* GFP_ATOMIC: the hook may run with no sleeping allowed. */
	w = kzalloc(sizeof(*w), GFP_ATOMIC);
	if (!w)
		goto drop;

	w->dentry = dget(dentry);
	w->idmap = idmap;
	strscpy(w->session, session, sizeof(w->session));
	strscpy(w->ts, ts, sizeof(w->ts));
	/* v0.4: best-effort, may be NULL or empty — strscpy("") is fine. */
	strscpy(w->tool, tool ? tool : "", sizeof(w->tool));
	strscpy(w->turn, turn ? turn : "", sizeof(w->turn));
	strscpy(w->intent, intent ? intent : "", sizeof(w->intent));
	INIT_WORK(&w->work, provfs_stamp_worker);

	/*
	 * Bump depth before queueing so a concurrent enqueue sees the
	 * slot consumed; the worker decrements after the write. A racing
	 * over-admit can transiently exceed queue_max by the number of
	 * concurrent enqueuers, which is fine for a best-effort bound.
	 */
	atomic_inc(&provfs_queue_depth);
	if (!queue_work(provfs_wq, &w->work)) {
		/* Already queued (can't happen for a fresh struct) — undo. */
		atomic_dec(&provfs_queue_depth);
		dput(w->dentry);
		kfree(w);
		goto drop;
	}
	return;

drop:
	atomic64_inc(&provfs_queue_dropped);
}

int provfs_work_init(void)
{
	provfs_wq = alloc_workqueue("provfs_stamp",
				    WQ_UNBOUND | WQ_MEM_RECLAIM, 0);
	if (!provfs_wq)
		return -ENOMEM;

	provfs_sysctl_hdr = register_sysctl("kernel/provfs",
					    provfs_sysctl_table);
	if (!provfs_sysctl_hdr)
		pr_warn("provfs: failed to register kernel/provfs sysctl table\n");

	return 0;
}

void provfs_work_exit(void)
{
	if (provfs_sysctl_hdr) {
		unregister_sysctl_table(provfs_sysctl_hdr);
		provfs_sysctl_hdr = NULL;
	}
	if (provfs_wq) {
		/* Drains pending work synchronously. */
		destroy_workqueue(provfs_wq);
		provfs_wq = NULL;
	}
}
