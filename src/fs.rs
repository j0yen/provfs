//! The provfs passthrough filesystem.
//!
//! Strategy: every operation maps a FUSE inode to a real path under the
//! backing `source` directory, then issues the corresponding syscall.
//! On every write-path operation (`create`, `write+release`, `setattr`,
//! `mkdir`, `rename`-into) we resolve the caller's [`Identity`] from
//! its pid and stamp the canonical `user.prov.*` xattrs.
//!
//! ## Status of this scaffold
//!
//! - Pure-function stamping pipeline ([`should_stamp`], [`stamp_now`]) is
//!   fully implemented and tested.
//! - Inode <-> path table ([`InodeTable`]) is implemented.
//! - The `fuser::Filesystem` trait impl implements `lookup`, `getattr`,
//!   `read`, `write`, `create`, `release`, `setattr`, `readdir`,
//!   `mkdir`, `unlink`, `rmdir`. Other ops fall through to the default
//!   ENOSYS — those are TODOs for a follow-up pass (symlink, link,
//!   rename, statfs, fsync, getxattr, setxattr, listxattr).
//! - The FUSE impl is deliberately conservative: it does not cache
//!   attrs, does not implement direct-io, and uses 1s attr TTLs.
//!
//! This is enough to mount, browse, edit, and observe stamping. It is
//! NOT yet production-ready; expect performance to be poor under
//! parallel workloads and certain syscalls (renames, hard links) to
//! return ENOSYS until filled in.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use libc::c_int;
use parking_lot::Mutex;

use crate::identity::{Identity, resolve_identity};
use crate::skip::SkipList;
use crate::xattrs;

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;

/// Inode table mapping FUSE ino -> absolute path under `source`.
#[derive(Debug, Default)]
pub struct InodeTable {
    fwd: HashMap<u64, PathBuf>,
    rev: HashMap<PathBuf, u64>,
    next: u64,
}

impl InodeTable {
    /// Create a new table with the root inode reserved.
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        let mut t = Self {
            fwd: HashMap::new(),
            rev: HashMap::new(),
            next: ROOT_INO + 1,
        };
        t.fwd.insert(ROOT_INO, root.clone());
        t.rev.insert(root, ROOT_INO);
        t
    }

    /// Look up the path for a given inode.
    #[must_use]
    pub fn path(&self, ino: u64) -> Option<&Path> {
        self.fwd.get(&ino).map(PathBuf::as_path)
    }

    /// Get-or-allocate the inode for the given path.
    pub fn intern(&mut self, p: PathBuf) -> u64 {
        if let Some(&ino) = self.rev.get(&p) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.fwd.insert(ino, p.clone());
        self.rev.insert(p, ino);
        ino
    }

    /// Forget a path (e.g. after unlink / rmdir).
    pub fn forget_path(&mut self, p: &Path) {
        if let Some(ino) = self.rev.remove(p) {
            self.fwd.remove(&ino);
        }
    }
}

/// Decide whether a path should be stamped, given the skip list.
///
/// `relpath` is the path relative to the FUSE mount source root.
#[must_use]
pub fn should_stamp(relpath: &Path, skip: &SkipList) -> bool {
    !skip.should_skip(relpath)
}

/// Resolve identity for `pid` and stamp `abspath` with the current time.
///
/// Errors from the underlying xattr syscalls are logged at warn level but
/// not propagated — provenance is a hint layer, not a hard requirement.
pub fn stamp_now(abspath: &Path, pid: u32) {
    let id = resolve_identity(pid);
    let now = Utc::now().to_rfc3339();
    if let Err(e) = xattrs::stamp(abspath, &id, &now) {
        log::warn!(
            target: "provfs::fs",
            "stamp failed for {} (session={}): {}",
            abspath.display(),
            id.session,
            e
        );
    }
}

/// Stamp `abspath` with the given identity (used in tests).
pub fn stamp_with(abspath: &Path, id: &Identity, now_iso: &str) -> std::io::Result<()> {
    xattrs::stamp(abspath, id, now_iso)
}

/// The provfs `Filesystem` itself.
#[allow(dead_code)]
pub struct ProvFs {
    source: PathBuf,
    skip: Arc<SkipList>,
    inodes: Mutex<InodeTable>,
    // dirty[ino] = true if release() should re-stamp
    dirty: Mutex<HashMap<u64, ()>>,
}

impl ProvFs {
    /// Construct a new provfs over the given backing directory.
    #[must_use]
    pub fn new(source: PathBuf, skip: SkipList) -> Self {
        let inodes = InodeTable::with_root(source.clone());
        Self {
            source,
            skip: Arc::new(skip),
            inodes: Mutex::new(inodes),
            dirty: Mutex::new(HashMap::new()),
        }
    }

    fn relpath(&self, abs: &Path) -> PathBuf {
        abs.strip_prefix(&self.source).map_or_else(|_| abs.to_path_buf(), Path::to_path_buf)
    }

    fn maybe_stamp(&self, abs: &Path, pid: u32) {
        let rel = self.relpath(abs);
        if should_stamp(&rel, &self.skip) {
            stamp_now(abs, pid);
        }
    }
}

fn libc_stat_to_attr(ino: u64, st: &libc::stat) -> FileAttr {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mk = |secs: i64, nanos: i64| -> SystemTime {
        let nanos_u32 = u32::try_from(nanos.max(0)).unwrap_or(0);
        if secs >= 0 {
            let secs_u64 = u64::try_from(secs).unwrap_or(0);
            UNIX_EPOCH + Duration::new(secs_u64, nanos_u32)
        } else {
            UNIX_EPOCH
        }
    };
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    };
    let size = u64::try_from(st.st_size).unwrap_or(0);
    let blocks = u64::try_from(st.st_blocks).unwrap_or(0);
    let nlink = u32::try_from(st.st_nlink).unwrap_or(1);
    FileAttr {
        ino,
        size,
        blocks,
        atime: mk(st.st_atime, st.st_atime_nsec),
        mtime: mk(st.st_mtime, st.st_mtime_nsec),
        ctime: mk(st.st_ctime, st.st_ctime_nsec),
        crtime: mk(st.st_ctime, st.st_ctime_nsec),
        kind,
        perm: u16::try_from(st.st_mode & 0o7777).unwrap_or(0o644),
        nlink,
        uid: st.st_uid,
        gid: st.st_gid,
        rdev: u32::try_from(st.st_rdev).unwrap_or(0),
        blksize: u32::try_from(st.st_blksize).unwrap_or(4096),
        flags: 0,
    }
}

fn lstat(p: &Path) -> std::io::Result<libc::stat> {
    use std::os::unix::ffi::OsStrExt;
    let cstr = std::ffi::CString::new(p.as_os_str().as_bytes())?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::lstat(cstr.as_ptr(), &raw mut st) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

fn io_err_to_errno(e: &std::io::Error) -> c_int {
    e.raw_os_error().unwrap_or(libc::EIO)
}

impl Filesystem for ProvFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let mut inodes = self.inodes.lock();
        let Some(parent_path) = inodes.path(parent).map(Path::to_path_buf) else {
            reply.error(libc::ENOENT);
            return;
        };
        let child = parent_path.join(name);
        match lstat(&child) {
            Ok(st) => {
                let ino = inodes.intern(child);
                let attr = libc_stat_to_attr(ino, &st);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(io_err_to_errno(&e)),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let inodes = self.inodes.lock();
        let Some(path) = inodes.path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match lstat(path) {
            Ok(st) => reply.attr(&TTL, &libc_stat_to_attr(ino, &st)),
            Err(e) => reply.error(io_err_to_errno(&e)),
        }
    }

    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let mut inodes = self.inodes.lock();
        let Some(parent_path) = inodes.path(parent).map(Path::to_path_buf) else {
            reply.error(libc::ENOENT);
            return;
        };
        let child = parent_path.join(name);

        use std::os::unix::ffi::OsStrExt;
        let cstr = match std::ffi::CString::new(child.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let fd =
            unsafe { libc::open(cstr.as_ptr(), flags | libc::O_CREAT, libc::c_uint::from(mode)) };
        if fd < 0 {
            reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            return;
        }

        let st = match lstat(&child) {
            Ok(s) => s,
            Err(e) => {
                unsafe { libc::close(fd) };
                reply.error(io_err_to_errno(&e));
                return;
            }
        };
        let ino = inodes.intern(child.clone());
        drop(inodes);

        // Stamp on initial create — this is the canonical "who made this file"
        // event; the subsequent write+release loop will refresh.
        self.maybe_stamp(&child, req.pid());
        self.dirty.lock().insert(ino, ());

        let attr = libc_stat_to_attr(ino, &st);
        let fh = u64::try_from(fd).unwrap_or(0);
        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let inodes = self.inodes.lock();
        let Some(path) = inodes.path(ino).map(Path::to_path_buf) else {
            reply.error(libc::ENOENT);
            return;
        };
        drop(inodes);

        use std::os::unix::ffi::OsStrExt;
        let cstr = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let fd = unsafe { libc::open(cstr.as_ptr(), flags, 0) };
        if fd < 0 {
            reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            return;
        }
        // Mark dirty if opened writable, so release() re-stamps.
        let writable = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        if writable {
            self.dirty.lock().insert(ino, ());
        }
        reply.opened(u64::try_from(fd).unwrap_or(0), 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let mut buf = vec![0u8; size as usize];
        let fd = i32::try_from(fh).unwrap_or(-1);
        if fd < 0 {
            reply.error(libc::EBADF);
            return;
        }
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                size as usize,
                offset,
            )
        };
        if n < 0 {
            reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            return;
        }
        let n_usize = usize::try_from(n).unwrap_or(0);
        buf.truncate(n_usize);
        reply.data(&buf);
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let fd = i32::try_from(fh).unwrap_or(-1);
        if fd < 0 {
            reply.error(libc::EBADF);
            return;
        }
        let n = unsafe {
            libc::pwrite(
                fd,
                data.as_ptr().cast::<libc::c_void>(),
                data.len(),
                offset,
            )
        };
        if n < 0 {
            reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            return;
        }
        self.dirty.lock().insert(ino, ());
        reply.written(u32::try_from(n).unwrap_or(0));
    }

    fn release(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let fd = i32::try_from(fh).unwrap_or(-1);
        if fd >= 0 {
            unsafe {
                libc::close(fd);
            }
        }
        let was_dirty = self.dirty.lock().remove(&ino).is_some();
        if was_dirty {
            let path = self.inodes.lock().path(ino).map(Path::to_path_buf);
            if let Some(p) = path {
                self.maybe_stamp(&p, req.pid());
            }
        }
        reply.ok();
    }

    fn setattr(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let inodes = self.inodes.lock();
        let Some(path) = inodes.path(ino).map(Path::to_path_buf) else {
            reply.error(libc::ENOENT);
            return;
        };
        drop(inodes);
        use std::os::unix::ffi::OsStrExt;
        let cstr = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        if let Some(m) = mode {
            let rc = unsafe { libc::chmod(cstr.as_ptr(), m as libc::mode_t) };
            if rc < 0 {
                reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
                return;
            }
        }
        if let Some(sz) = size {
            let rc = unsafe { libc::truncate(cstr.as_ptr(), sz as libc::off_t) };
            if rc < 0 {
                reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
                return;
            }
        }
        // setattr changes the file → stamp.
        self.maybe_stamp(&path, req.pid());

        match lstat(&path) {
            Ok(st) => reply.attr(&TTL, &libc_stat_to_attr(ino, &st)),
            Err(e) => reply.error(io_err_to_errno(&e)),
        }
    }

    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let mut inodes = self.inodes.lock();
        let Some(parent_path) = inodes.path(parent).map(Path::to_path_buf) else {
            reply.error(libc::ENOENT);
            return;
        };
        let child = parent_path.join(name);
        use std::os::unix::ffi::OsStrExt;
        let cstr = match std::ffi::CString::new(child.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let rc = unsafe { libc::mkdir(cstr.as_ptr(), mode as libc::mode_t) };
        if rc < 0 {
            reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            return;
        }
        let st = match lstat(&child) {
            Ok(s) => s,
            Err(e) => {
                reply.error(io_err_to_errno(&e));
                return;
            }
        };
        let ino = inodes.intern(child.clone());
        drop(inodes);
        self.maybe_stamp(&child, req.pid());
        reply.entry(&TTL, &libc_stat_to_attr(ino, &st), 0);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = {
            let inodes = self.inodes.lock();
            inodes.path(parent).map(Path::to_path_buf)
        };
        let Some(parent_path) = parent_path else {
            reply.error(libc::ENOENT);
            return;
        };
        let child = parent_path.join(name);
        use std::os::unix::ffi::OsStrExt;
        let cstr = match std::ffi::CString::new(child.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let rc = unsafe { libc::unlink(cstr.as_ptr()) };
        if rc < 0 {
            reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            return;
        }
        self.inodes.lock().forget_path(&child);
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = {
            let inodes = self.inodes.lock();
            inodes.path(parent).map(Path::to_path_buf)
        };
        let Some(parent_path) = parent_path else {
            reply.error(libc::ENOENT);
            return;
        };
        let child = parent_path.join(name);
        use std::os::unix::ffi::OsStrExt;
        let cstr = match std::ffi::CString::new(child.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let rc = unsafe { libc::rmdir(cstr.as_ptr()) };
        if rc < 0 {
            reply.error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            return;
        }
        self.inodes.lock().forget_path(&child);
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let inodes = self.inodes.lock();
        let Some(path) = inodes.path(ino).map(Path::to_path_buf) else {
            reply.error(libc::ENOENT);
            return;
        };
        drop(inodes);

        let entries = match std::fs::read_dir(&path) {
            Ok(e) => e,
            Err(e) => {
                reply.error(io_err_to_errno(&e));
                return;
            }
        };

        let mut all: Vec<(u64, FileType, OsString)> = Vec::new();
        // POSIX always-present entries:
        all.push((ino, FileType::Directory, OsString::from(".")));
        all.push((ino, FileType::Directory, OsString::from("..")));
        for ent in entries.flatten() {
            let ftype = ent
                .file_type()
                .map(|t| {
                    if t.is_dir() {
                        FileType::Directory
                    } else if t.is_symlink() {
                        FileType::Symlink
                    } else {
                        FileType::RegularFile
                    }
                })
                .unwrap_or(FileType::RegularFile);
            let child_path = ent.path();
            let child_ino = self.inodes.lock().intern(child_path);
            all.push((child_ino, ftype, ent.file_name()));
        }

        let offset_usize = usize::try_from(offset).unwrap_or(0);
        for (i, (cino, ftype, name)) in all.into_iter().enumerate().skip(offset_usize) {
            let next_off = i64::try_from(i + 1).unwrap_or(0);
            if reply.add(cino, next_off, ftype, &name) {
                break;
            }
        }
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn inode_table_assigns_unique_inodes() {
        let dir = TempDir::new().unwrap();
        let mut t = InodeTable::with_root(dir.path().to_path_buf());
        assert_eq!(t.path(ROOT_INO).unwrap(), dir.path());

        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let ino_a = t.intern(a.clone());
        let ino_b = t.intern(b.clone());
        assert_ne!(ino_a, ino_b);
        assert_ne!(ino_a, ROOT_INO);

        // Same path reuses the inode.
        assert_eq!(t.intern(a), ino_a);
    }

    #[test]
    fn inode_table_forget_drops_both_directions() {
        let dir = TempDir::new().unwrap();
        let mut t = InodeTable::with_root(dir.path().to_path_buf());
        let a = dir.path().join("a");
        let ino = t.intern(a.clone());
        assert!(t.path(ino).is_some());
        t.forget_path(&a);
        assert!(t.path(ino).is_none());
        // Re-interning yields a fresh inode (forward and reverse both gone).
        let new = t.intern(a);
        assert_ne!(new, ino);
    }

    #[test]
    fn should_stamp_respects_skiplist() {
        let s = SkipList::defaults();
        assert!(should_stamp(Path::new("notes.md"), &s));
        assert!(!should_stamp(Path::new(".git/HEAD"), &s));
    }
}
