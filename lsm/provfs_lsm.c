// SPDX-License-Identifier: GPL-2.0
/*
 * provfs LSM — stamp user.prov.* xattrs on writes.
 *
 * v0.1: hooks file_release. If the file was opened for write
 * (file->f_mode & FMODE_WRITE), build an identity string from
 * comm:<comm>:pid:<tgid>:uid:<uid> and stamp:
 *
 *   user.prov.session = "comm:<comm>:pid:<tgid>:uid:<uid>"
 *   user.prov.ts      = "<unix_seconds>"
 *
 * v0.2 (PRD-provfs-deferred-stamp): the actual xattr writes are
 * deferred to a bounded workqueue (provfs_work.c). The hook does only
 * the cheap work — guards, path resolution, skip filter, session+ts
 * rendering — then calls provfs_enqueue_stamp() and returns. This keeps
 * journaled filesystem writes off the close hot path and off the
 * task-teardown context (fput from exit_files()).
 *
 * v0.3 (PRD-provfs-comm-richer): the fallback path (agentns session id
 * absent) is enriched. Instead of "comm:<comm>:pid:<pid>:uid:<uid>",
 * which names the innermost child of a pipeline (awk/sed/install) rather
 * than the meaningful actor, the fallback composes a structured value:
 *
 *   comm-chain:<comm0>>...>;env:<KEY>=<val>;cwd:<path>;pid:<pid>;uid:<uid>
 *
 * Fields are key:value pairs separated by ';' in a fixed order
 * (comm-chain, env, cwd, pid, uid). Any field may be absent; pid+uid are
 * always present (cheap, never fail). The whole value is capped at
 * PROV_IDENT_MAX bytes; truncation drops fields from the right so the
 * outermost-actor signal (comm-chain) is preserved per §2.2.
 *
 * All of this is rendered at hook time (in provfs_build_session, called
 * from provfs_stamp while still in the writer's task context) and copied
 * into the work payload's session[] field. The deferred worker never
 * re-reads the originating task — exactly the §1.3 hook-time capture
 * buffer the deferred-stamp PRD anticipated. current->mm and current->fs
 * are read best-effort, each guarded; on teardown the field is omitted.
 *
 * The agentns-present path is unchanged: still the bare 32-hex id.
 *
 * v0.4 (Phase 1, PRD-provenance-fs.md §4.1): two more independent
 * additions.
 *
 * First, three more xattrs read from the writer's environment —
 * user.prov.tool ($CLAUDE_TOOL), user.prov.turn ($CLAUDE_TURN), and
 * user.prov.intent ($AGENTNS_INTENT) — each the bare env value
 * (truncated to PROV_FIELD_MAX-1), stamped only when non-empty. These
 * share the single access_remote_vm() pass the v0.3 enriched fallback
 * already made over the env block: provfs_scan_env() takes a small
 * table of {candidate keys, dest, destlen, keep-"KEY="-prefix} targets
 * and fills every one of them from one walk of the block, so the v0.3
 * fallback's "env:<KEY>=<val>" field and these three new bare fields
 * come out of the same read. The v0.3 fallback's own selection (first
 * matching env-block entry, in block order) is unchanged byte for
 * byte — see provfs_scan_writer_env().
 *
 * Second, the skip-prefix list is no longer hardcoded: it is a
 * runtime-tunable, comma-separated string at
 * /proc/sys/kernel/provfs/skip_prefixes (see provfs_work.c), guarded
 * there by a rwlock so a concurrent sysctl write can't tear a match
 * taken from this hook's task context.
 *
 * Phase 2 (deferred): history ring (user.prov.history).
 *
 * Per PRD-provenance-fs.md §4.1, paired with the existing FUSE-side
 * Rust crate at ~/wintermute/provfs/, and with the separately-shipped
 * `prov` userspace CLI.
 */

#include <linux/dcache.h>
#include <linux/fs.h>
#include <linux/fs_struct.h>	/* get_fs_pwd() for the cwd field */
#include <linux/init.h>
#include <linux/kernel.h>
#include <linux/lsm_hooks.h>
#include <linux/mm.h>		/* access_remote_vm() for the env field */
#include <linux/mnt_idmapping.h>
#include <linux/mount.h>
#include <linux/nsproxy.h>
#include <linux/path.h>
#include <linux/rcupdate.h>	/* rcu_dereference_protected in comm-chain walk */
#include <linux/sched.h>
#include <linux/sched/mm.h>	/* get_task_mm()/mmput() */
#include <linux/sched/task.h>	/* tasklist_lock / real_parent walk */
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uaccess.h>
#include <linux/uidgid.h>
#include <linux/xattr.h>

#include "provfs_work.h"

#ifdef CONFIG_AGENT_NS
#include <linux/agent_namespaces.h>
#endif

#define PROVFS_NAME		"provfs"

/* Enrichment sub-field caps (PRD-provfs-comm-richer §2.1). */
#define PROV_CHAIN_MAX		128	/* "comm0>comm1>comm2" + slack */
#define PROV_CHAIN_LEVELS	3	/* current + up to 2 ancestors */
#define PROV_ENV_KEY_MAX	24
#define PROV_ENV_VAL_MAX	48
#define PROV_ENV_SCAN_MAX	4096	/* env bytes we are willing to scan */

/*
 * Env vars for the v0.3 enriched-fallback "env:" field, in priority
 * order; first match (in env-block order, see provfs_scan_env()) wins
 * (§2.1, §4). Unchanged by v0.4.
 */
static const char * const provfs_env_keys[] = {
	"CLAUDE_TOOL=",
	"AGORABUS_SID=",
	"CLAUDE_SESSION_ID=",
	NULL,
};

/* v0.4 (Phase 1): one specific env var per new xattr field. */
static const char * const provfs_key_tool[] = { "CLAUDE_TOOL=", NULL };
static const char * const provfs_key_turn[] = { "CLAUDE_TURN=", NULL };
static const char * const provfs_key_intent[] = { "AGENTNS_INTENT=", NULL };

/*
 * Skip-prefix matching is now sysctl-tunable (v0.4,
 * /proc/sys/kernel/provfs/skip_prefixes) — see provfs_path_skipped() in
 * provfs_work.c, declared in provfs_work.h.
 */

/* True for the synthetic roots we stop the parent-walk at (§2.1). */
static bool provfs_is_root_comm(const char *comm)
{
	return !strcmp(comm, "init") || !strcmp(comm, "systemd") ||
	       !strcmp(comm, "kthreadd");
}

/*
 * Build "comm0>comm1>comm2" by walking current->real_parent up to
 * PROV_CHAIN_LEVELS levels. Stops early when the current/next ancestor
 * is init/systemd/kthreadd (a system root carries no actor signal). The
 * walk holds the tasklist read-lock so real_parent can't be freed
 * mid-walk; get_task_comm copies under the task's own lock. Best
 * effort — on any oddity we just emit what we have so far.
 *
 * Returns the number of bytes written (excluding NUL), 0 if nothing.
 */
static size_t provfs_build_comm_chain(char *buf, size_t buflen)
{
	struct task_struct *t = current;
	char comm[TASK_COMM_LEN];
	size_t off = 0;
	int level;

	if (!buf || buflen == 0)
		return 0;
	buf[0] = '\0';

	read_lock(&tasklist_lock);
	for (level = 0; level < PROV_CHAIN_LEVELS && t; level++) {
		int n;

		get_task_comm(comm, t);

		/*
		 * Don't lead with a root comm, and don't append one as an
		 * ancestor — stop the chain at the first meaningful boundary.
		 */
		if (provfs_is_root_comm(comm))
			break;

		n = snprintf(buf + off, buflen - off, "%s%s",
			     off ? ">" : "", comm);
		if (n < 0 || (size_t)n >= buflen - off) {
			/* Out of room; keep what fit. */
			break;
		}
		off += n;

		t = rcu_dereference_protected(t->real_parent,
					      lockdep_is_held(&tasklist_lock));
		/* PID 1 / swapper sentinel: nothing useful above it. */
		if (t && (t->pid == 1 || t->pid == 0))
			break;
	}
	read_unlock(&tasklist_lock);

	return off;
}

/*
 * One "find this env var, write it here" request for provfs_scan_env().
 * @keys is a NULL-terminated list of candidate "KEY=" prefixes, checked
 * in list order against each env entry; @dest/@destlen is where the
 * match is rendered. @keep_prefix true renders "<KEY>=<val>" (KEY
 * without the trailing '=', value capped at PROV_ENV_VAL_MAX — the v0.3
 * enriched-fallback form); false renders the bare value only, truncated
 * to fit @destlen (the v0.4 Phase-1 fields). @filled is scanner-owned
 * state: once true, this target is skipped for the rest of the scan, so
 * whichever env-block entry matches it first is never overwritten by a
 * later one — this is what makes the v0.3 fallback's selection (first
 * matching entry in block order) come out byte-for-byte identical
 * whether it is the only target scanned or one of several.
 */
struct provfs_env_target {
	const char * const *keys;
	char *dest;
	size_t destlen;
	bool keep_prefix;
	bool filled;
};

/*
 * Single pass over the current task's environment, filling every
 * not-yet-filled target in @targets whose key is found. Best effort:
 * a target simply stays unfilled (caller pre-clears dest) if mm is
 * gone, the env region is unreadable, or nothing matches. Reads at
 * most PROV_ENV_SCAN_MAX bytes of the env block via access_remote_vm().
 */
static void provfs_scan_env(struct provfs_env_target *targets, size_t ntargets)
{
	struct mm_struct *mm;
	char *env = NULL;
	unsigned long start, end, len;
	size_t got;
	size_t i;

	mm = get_task_mm(current);
	if (!mm)
		return;

	/*
	 * env_start/env_end bound argv's trailing environ block. They can
	 * be zero or inverted on odd execs; guard before subtracting.
	 */
	start = mm->env_start;
	end = mm->env_end;
	if (!start || !end || end <= start)
		goto out_mm;

	len = end - start;
	if (len > PROV_ENV_SCAN_MAX)
		len = PROV_ENV_SCAN_MAX;

	env = kmalloc(len + 1, GFP_KERNEL);
	if (!env)
		goto out_mm;

	got = access_remote_vm(mm, start, env, len, 0);
	if (got == 0)
		goto out_free;
	env[got] = '\0';

	/*
	 * The env block is a run of NUL-separated "KEY=value" entries.
	 * Walk entry by entry; for each, test every target still open.
	 */
	i = 0;
	while (i < got) {
		const char *entry = env + i;
		size_t elen = strnlen(entry, got - i);
		size_t t;

		for (t = 0; t < ntargets; t++) {
			struct provfs_env_target *tgt = &targets[t];
			const char * const *k;

			if (tgt->filled)
				continue;

			for (k = tgt->keys; *k; k++) {
				size_t klen = strlen(*k);
				const char *val;

				if (elen <= klen || strncmp(entry, *k, klen))
					continue;

				val = entry + klen;
				if (tgt->keep_prefix) {
					/* Drop the trailing '=' from the key label. */
					char key[PROV_ENV_KEY_MAX];
					size_t kn = klen - 1;

					if (kn >= sizeof(key))
						kn = sizeof(key) - 1;
					memcpy(key, *k, kn);
					key[kn] = '\0';

					scnprintf(tgt->dest, tgt->destlen, "%s=%.*s",
						  key, PROV_ENV_VAL_MAX, val);
				} else {
					scnprintf(tgt->dest, tgt->destlen, "%s", val);
				}
				tgt->filled = true;
				break;
			}
		}

		/* Advance past this entry's NUL terminator. */
		i += elen + 1;
	}

out_free:
	kfree(env);
out_mm:
	mmput(mm);
}

/*
 * Fill the v0.3 enriched-fallback "env:" field and the three v0.4
 * Phase-1 fields (tool/turn/intent) from one provfs_scan_env() pass.
 * Any/all may come back empty (env var absent, or mm gone/unreadable).
 */
static void provfs_scan_writer_env(char *envsig, size_t envsiglen,
				    char *tool, size_t toollen,
				    char *turn, size_t turnlen,
				    char *intent, size_t intentlen)
{
	struct provfs_env_target targets[] = {
		{ provfs_env_keys,   envsig, envsiglen, true,  false },
		{ provfs_key_tool,   tool,   toollen,   false, false },
		{ provfs_key_turn,   turn,   turnlen,   false, false },
		{ provfs_key_intent, intent, intentlen, false, false },
	};

	envsig[0] = '\0';
	tool[0] = '\0';
	turn[0] = '\0';
	intent[0] = '\0';

	provfs_scan_env(targets, ARRAY_SIZE(targets));
}

/*
 * Render the writer's cwd via get_fs_pwd() + d_absolute_path() — the
 * same root-relative renderer provfs_stamp() uses for the file path, so
 * it is safe even when current->fs is being torn down (it takes its own
 * reference). Returns bytes written, 0 on failure.
 */
static size_t provfs_build_cwd(char *buf, size_t buflen)
{
	struct path pwd;
	char *page;
	char *p;
	size_t out = 0;

	if (!buf || buflen == 0)
		return 0;
	buf[0] = '\0';

	if (!current->fs)
		return 0;
	get_fs_pwd(current->fs, &pwd);
	if (!pwd.dentry || !pwd.mnt) {
		path_put(&pwd);
		return 0;
	}

	page = kmalloc(PATH_MAX, GFP_KERNEL);
	if (!page) {
		path_put(&pwd);
		return 0;
	}

	p = d_absolute_path(&pwd, page, PATH_MAX);
	if (!IS_ERR_OR_NULL(p))
		out = scnprintf(buf, buflen, "%s", p);

	kfree(page);
	path_put(&pwd);
	return out;
}

/*
 * Compose the enriched fallback value (agentns absent). Fixed field
 * order: comm-chain, env, cwd, pid, uid. Each field is appended only if
 * it fits in full within @buflen; pid+uid always fit (the buffer is
 * sized for them). This drops fields from the right on overflow, which
 * preserves the outermost-actor signal at the front per §2.2.
 */
static void provfs_build_fallback(char *buf, size_t buflen, const char *envsig)
{
	char chain[PROV_CHAIN_MAX];
	char *cwd;
	u32 uid = from_kuid(&init_user_ns, current_uid());
	u32 pid = (u32)current->tgid;
	size_t off = 0;
	int n;

	buf[0] = '\0';

	/* comm-chain (outermost actor signal — emitted first). */
	if (provfs_build_comm_chain(chain, sizeof(chain)) > 0) {
		n = snprintf(buf + off, buflen - off, "%scomm-chain:%s",
			     off ? ";" : "", chain);
		if (n > 0 && (size_t)n < buflen - off)
			off += n;
	}

	/*
	 * env signal (CLAUDE_TOOL / AGORABUS_SID / CLAUDE_SESSION_ID).
	 * @envsig is pre-rendered by the caller's single provfs_scan_env()
	 * pass (v0.4) — see provfs_scan_writer_env(); this is the same
	 * value the old (now-folded-in) single-purpose scan used to render.
	 */
	if (envsig && envsig[0]) {
		n = snprintf(buf + off, buflen - off, "%senv:%s",
			     off ? ";" : "", envsig);
		if (n > 0 && (size_t)n < buflen - off)
			off += n;
	}

	/* cwd. PATH_MAX is large; heap-allocate to keep the stack frame small. */
	cwd = kmalloc(PATH_MAX, GFP_KERNEL);
	if (cwd) {
		if (provfs_build_cwd(cwd, PATH_MAX) > 0) {
			n = snprintf(buf + off, buflen - off, "%scwd:%s",
				     off ? ";" : "", cwd);
			if (n > 0 && (size_t)n < buflen - off)
				off += n;
		}
		kfree(cwd);
	}

	/* pid (always). */
	n = snprintf(buf + off, buflen - off, "%spid:%u",
		     off ? ";" : "", pid);
	if (n > 0 && (size_t)n < buflen - off)
		off += n;

	/* uid (always). */
	n = snprintf(buf + off, buflen - off, "%suid:%u",
		     off ? ";" : "", uid);
	if (n > 0 && (size_t)n < buflen - off)
		off += n;

	/*
	 * Defensive: if pid/uid somehow couldn't fit (pathological buflen),
	 * guarantee a non-empty, parseable value.
	 */
	if (buf[0] == '\0')
		snprintf(buf, buflen, "pid:%u;uid:%u", pid, uid);
}

static void provfs_build_session(char *buf, size_t buflen, const char *envsig)
{
#ifdef CONFIG_AGENT_NS
	/*
	 * Prefer the AgentNS session id when the current task is inside a
	 * non-init agent namespace. The id is opaque 128 bits;
	 * agent_session_id_format renders it as a hex/UUID-ish string. This
	 * path is intentionally left UNCHANGED by PRD-provfs-comm-richer —
	 * enrichment applies only to the fallback below.
	 */
	if (current->nsproxy && current->nsproxy->agent_ns &&
	    current->nsproxy->agent_ns != &init_agent_ns) {
		int n = agent_session_id_format(
			&current->nsproxy->agent_ns->session_id, buf, buflen);
		if (n > 0)
			return;
	}
#endif
	/*
	 * Fallback (PRD-provfs-comm-richer): agentns id absent. Compose the
	 * enriched comm-chain;env;cwd;pid;uid value instead of the old
	 * innermost-comm-only string. Rendered here at hook time (writer's
	 * task context) and carried verbatim through the work payload.
	 */
	provfs_build_fallback(buf, buflen, envsig);
}

/*
 * Hook-side: do the cheap work and enqueue. Called from file_release
 * for writes; the calling task may be exiting (fput from exit_files),
 * so we must not touch current->fs.
 *
 * The journaled xattr writes themselves happen later, on the provfs
 * workqueue (see provfs_work.c).
 */
static void provfs_stamp(struct file *file)
{
	struct dentry *dentry;
	struct mnt_idmap *idmap;
	struct inode *inode;
	char *path_buf;
	char *path_str;
	char session_val[PROV_IDENT_MAX];
	char ts_val[PROV_TS_MAX];
	char envsig_val[PROV_ENV_KEY_MAX + PROV_ENV_VAL_MAX + 2];
	char tool_val[PROV_FIELD_MAX];
	char turn_val[PROV_FIELD_MAX];
	char intent_val[PROV_FIELD_MAX];

	/*
	 * file_release fires during fput, including from exit_files() after
	 * exit_fs() has cleared current->fs. d_path() consults
	 * current->fs->root and NULL-derefs in that window — observed as
	 * d_path+0xa2 -> provfs_stamp+0x129 oopses. Use d_absolute_path(),
	 * which renders the path relative to the global root and never
	 * reads current->fs.
	 *
	 * Note: provfs_build_session()'s enriched fallback (v0.3) does read
	 * current->fs (cwd) and current->mm (env), but each via a guarded
	 * accessor that takes its own reference and omits the field when the
	 * struct is being torn down — so a half-exited task degrades to a
	 * partial (still parseable) value rather than oopsing.
	 */
	if (!file->f_path.mnt || !file->f_path.dentry)
		return;
	dentry = file_dentry(file);
	if (!dentry)
		return;
	inode = file_inode(file);
	if (!inode || !S_ISREG(inode->i_mode))
		return;
	idmap = file_mnt_idmap(file);

	path_buf = kmalloc(PATH_MAX, GFP_KERNEL);
	if (!path_buf)
		return;
	path_str = d_absolute_path(&file->f_path, path_buf, PATH_MAX);
	if (IS_ERR_OR_NULL(path_str)) {
		kfree(path_buf);
		return;
	}
	if (provfs_path_skipped(path_str)) {
		kfree(path_buf);
		return;
	}
	kfree(path_buf);

	/*
	 * v0.4: one env scan feeds both the enriched fallback's "env:"
	 * field and the Phase-1 tool/turn/intent xattrs — see
	 * provfs_scan_writer_env(). Best effort throughout: a torn-down or
	 * unreadable mm just leaves every field empty.
	 */
	provfs_scan_writer_env(envsig_val, sizeof(envsig_val),
			       tool_val, sizeof(tool_val),
			       turn_val, sizeof(turn_val),
			       intent_val, sizeof(intent_val));

	provfs_build_session(session_val, sizeof(session_val), envsig_val);
	snprintf(ts_val, sizeof(ts_val), "%lld", ktime_get_real_seconds());

	/* Defer the journaled xattr writes off this hot/teardown path. */
	provfs_enqueue_stamp(dentry, idmap, session_val, ts_val,
			     tool_val, turn_val, intent_val);
}

static void provfs_file_release(struct file *file)
{
	if (!(file->f_mode & FMODE_WRITE))
		return;
	provfs_stamp(file);
}

static struct security_hook_list provfs_hooks[] __ro_after_init = {
	LSM_HOOK_INIT(file_release, provfs_file_release),
};

static const struct lsm_id provfs_lsmid = {
	.name = PROVFS_NAME,
	.id   = 119, /* arbitrary; LSM_ID_PROVFS — assign properly in upstream */
};

static int __init provfs_init(void)
{
	int ret;

	security_add_hooks(provfs_hooks, ARRAY_SIZE(provfs_hooks), &provfs_lsmid);

	ret = provfs_work_init();
	if (ret) {
		/*
		 * Hooks are already registered and __ro_after_init; we can't
		 * pull them back out. Without a workqueue every enqueue drops,
		 * so the LSM degrades to a no-op rather than crashing. Log
		 * loudly and carry on.
		 */
		pr_err("provfs: workqueue init failed (%d); stamping disabled\n",
		       ret);
		return ret;
	}

	pr_info("provfs: LSM registered (v0.4, Phase 1: tool/turn/intent keys + sysctl skip list)\n");
	return 0;
}

DEFINE_LSM(provfs) = {
	.id    = &provfs_lsmid,
	.init  = provfs_init,
	.order = LSM_ORDER_MUTABLE,
};
