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
 * Phase 1 (deferred): read $CLAUDE_TOOL / $CLAUDE_SESSION from
 * current->mm via access_remote_vm() and prefer those over the
 * comm-derived session id; add user.prov.tool / .turn / .intent.
 *
 * Phase 2 (deferred): history ring (user.prov.history).
 *
 * Skip prefix list is hardcoded for v0.1; sysctl tunable lands later.
 *
 * Per PRD-provenance-fs.md §4.1, paired with the existing FUSE-side
 * Rust crate at ~/wintermute/provfs/.
 */

#include <linux/dcache.h>
#include <linux/fs.h>
#include <linux/init.h>
#include <linux/kernel.h>
#include <linux/lsm_hooks.h>
#include <linux/mnt_idmapping.h>
#include <linux/mount.h>
#include <linux/nsproxy.h>
#include <linux/path.h>
#include <linux/sched.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uidgid.h>
#include <linux/xattr.h>

#ifdef CONFIG_AGENT_NS
#include <linux/agent_namespaces.h>
#endif

#define PROVFS_NAME		"provfs"
#define PROV_SESSION_KEY	XATTR_USER_PREFIX "prov.session"
#define PROV_TS_KEY		XATTR_USER_PREFIX "prov.ts"
#define PROV_IDENT_MAX		96	/* "comm:<TASK_COMM_LEN>:pid:<u32>:uid:<u32>" fits */
#define PROV_TS_MAX		24

/*
 * Hard-coded skip-prefix list (v0.1). The PRD calls for a sysctl-tunable
 * list; deferred to v0.2 to keep the initial change small.
 */
static const char * const provfs_skip_prefixes[] = {
	"/proc/", "/sys/", "/dev/", "/run/", "/tmp/",
	"/var/run/", "/var/cache/", "/var/lib/pacman/",
	"/.git/", "/node_modules/", "/target/", "/.cargo/registry/",
	NULL,
};

static bool provfs_path_skipped(const char *path)
{
	const char *const *p;

	if (!path)
		return true;
	for (p = provfs_skip_prefixes; *p; p++) {
		if (strstr(path, *p))
			return true;
	}
	return false;
}

static void provfs_build_session(char *buf, size_t buflen)
{
	char comm[TASK_COMM_LEN];
	u32 uid;

#ifdef CONFIG_AGENT_NS
	/*
	 * Phase 3: prefer the AgentNS session id when the current task is
	 * inside a non-init agent namespace. The id is opaque 128 bits;
	 * agent_session_id_format renders it as a hex/UUID-ish string.
	 */
	if (current->nsproxy && current->nsproxy->agent_ns &&
	    current->nsproxy->agent_ns != &init_agent_ns) {
		int n = agent_session_id_format(
			&current->nsproxy->agent_ns->session_id, buf, buflen);
		if (n > 0)
			return;
	}
#endif
	get_task_comm(comm, current);
	uid = from_kuid(&init_user_ns, current_uid());
	snprintf(buf, buflen, "comm:%s:pid:%u:uid:%u",
		 comm, (u32)current->tgid, uid);
}

/*
 * Stamp xattrs on the file. Called from file_release for writes;
 * the calling task may be exiting (fput from exit_files), so we must
 * not touch current->fs.
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

	/*
	 * file_release fires during fput, including from exit_files() after
	 * exit_fs() has cleared current->fs. d_path() consults
	 * current->fs->root and NULL-derefs in that window — observed as
	 * d_path+0xa2 -> provfs_stamp+0x129 oopses. Use d_absolute_path(),
	 * which renders the path relative to the global root and never
	 * reads current->fs.
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

	provfs_build_session(session_val, sizeof(session_val));
	snprintf(ts_val, sizeof(ts_val), "%lld", ktime_get_real_seconds());

	(void)__vfs_setxattr_noperm(idmap, dentry, PROV_SESSION_KEY,
				    session_val, strlen(session_val), 0);
	(void)__vfs_setxattr_noperm(idmap, dentry, PROV_TS_KEY,
				    ts_val, strlen(ts_val), 0);
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
	security_add_hooks(provfs_hooks, ARRAY_SIZE(provfs_hooks), &provfs_lsmid);
	pr_info("provfs: LSM registered (v0.1)\n");
	return 0;
}

DEFINE_LSM(provfs) = {
	.id    = &provfs_lsmid,
	.init  = provfs_init,
	.order = LSM_ORDER_MUTABLE,
};
