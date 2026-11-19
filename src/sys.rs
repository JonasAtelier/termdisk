// SPDX-License-Identifier: GPL-3.0-only

//! The Linux calls the scan and the trash guard share: folders opened as file descriptors
//! (never through a symlink), listed with `getdents64`, and entries lstat'ed with `statx`
//! relative to them. The calls go through `libc::syscall`, so a static musl build (as
//! termcode's) needs no libc wrapper for them.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::tree::{DIR, FILE, OTHER, SYMLINK};

/// What the scan and the guard need of an lstat.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// 512-byte blocks.
    pub blocks: u64,
    pub mtime: i64,
    pub uid: u32,
    pub nlink: u32,
    /// `tree::FILE`, `DIR`, `SYMLINK` or `OTHER`.
    pub kind: u16,
    /// The root of a mount: another filesystem, or a bind mount of the same one.
    pub mount_root: bool,
    /// Whether the kernel says if it is a mount root (Linux 5.8 and later).
    pub mount_root_known: bool,
}

/// lstat of a path, without triggering an automount.
pub(crate) fn stat_path(p: &Path) -> io::Result<Stat> {
    let c = CString::new(p.as_os_str().as_bytes())?;
    statx(
        libc::AT_FDCWD,
        &c,
        libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT,
    )
}

fn statx(fd: RawFd, name: &CStr, flags: i32) -> io::Result<Stat> {
    // SAFETY: an all-zero statx is valid; the kernel fills it in.
    let mut x: libc::statx = unsafe { std::mem::zeroed() };
    let flags = flags | libc::AT_STATX_SYNC_AS_STAT;
    // SAFETY: `name` is NUL-terminated and `x` is a statx the call may write.
    let r = unsafe {
        let x: *mut libc::statx = &mut x;
        let mask = libc::STATX_BASIC_STATS;
        libc::syscall(libc::SYS_statx, fd, name.as_ptr(), flags, mask, x)
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let attr = libc::STATX_ATTR_MOUNT_ROOT as u64;
    Ok(Stat {
        dev: libc::makedev(x.stx_dev_major, x.stx_dev_minor),
        ino: x.stx_ino,
        size: x.stx_size,
        blocks: x.stx_blocks,
        mtime: x.stx_mtime.tv_sec,
        uid: x.stx_uid,
        nlink: x.stx_nlink,
        kind: match u32::from(x.stx_mode) & libc::S_IFMT {
            libc::S_IFDIR => DIR,
            libc::S_IFLNK => SYMLINK,
            libc::S_IFREG => FILE,
            _ => OTHER,
        },
        mount_root: x.stx_attributes & attr != 0,
        mount_root_known: x.stx_attributes_mask & attr != 0,
    })
}

/// An open folder: `O_DIRECTORY | O_NOFOLLOW`, so it never comes through a symlink as its last
/// component. Opened `O_RDONLY` (to list it) or `O_PATH` (a place only).
pub(crate) struct Dir(OwnedFd);

impl Dir {
    /// Opens the folder at `path`. Earlier components are resolved as usual; to refuse a
    /// symlink anywhere, open from `/` one component at a time with `place_at`.
    pub fn open(path: &Path) -> io::Result<Dir> {
        let c = CString::new(path.as_os_str().as_bytes())?;
        open(libc::AT_FDCWD, &c, libc::O_RDONLY)
    }

    /// Opens the folder `name` in this one.
    pub fn open_at(&self, name: &CStr) -> io::Result<Dir> {
        open(self.fd(), name, libc::O_RDONLY)
    }

    /// `/`, opened as a place only (`O_PATH`), to walk down from with `place_at`.
    pub fn root_place() -> io::Result<Dir> {
        open(libc::AT_FDCWD, c"/", libc::O_PATH)
    }

    /// Opens the folder `name` in this one as a place only (`O_PATH`): it needs just search
    /// permission, and serves to stat, open and rename what is in it, but not to list it.
    pub fn place_at(&self, name: &CStr) -> io::Result<Dir> {
        open(self.fd(), name, libc::O_PATH)
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// The folder's own lstat.
    pub fn stat(&self) -> io::Result<Stat> {
        statx(
            self.fd(),
            c"",
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    }

    /// lstat of `name` in this folder, without triggering an automount.
    pub fn stat_at(&self, name: &CStr) -> io::Result<Stat> {
        statx(
            self.fd(),
            name,
            libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT,
        )
    }

    /// Calls `f` with the name of every entry but `.` and `..`, reading the folder in chunks
    /// into `buf`. Reads the folder once: a second call sees nothing.
    pub fn each(&self, buf: &mut Vec<u8>, mut f: impl FnMut(&CStr)) -> io::Result<()> {
        buf.resize(32 * 1024, 0);
        loop {
            // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
            let n = unsafe {
                let (fd, p, len) = (self.fd(), buf.as_mut_ptr(), buf.len());
                libc::syscall(libc::SYS_getdents64, fd, p, len)
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Ok(());
            }
            // linux_dirent64: d_ino u64, d_off i64, d_reclen u16, d_type u8, then the name
            // and its NUL.
            let mut off = 0;
            while off < n as usize {
                let reclen = u16::from_ne_bytes([buf[off + 16], buf[off + 17]]) as usize;
                let name = CStr::from_bytes_until_nul(&buf[off + 19..off + reclen])
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                if name != c"." && name != c".." {
                    f(name);
                }
                off += reclen;
            }
        }
    }
}

/// `openat` of a folder, never through a symlink as the last component (with `O_DIRECTORY`,
/// a symlink there fails with ENOTDIR, `O_PATH` or not).
fn open(fd: RawFd, name: &CStr, mode: i32) -> io::Result<Dir> {
    let flags = mode | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated.
    let r = unsafe { libc::openat(fd, name.as_ptr(), flags) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `r` is a new descriptor that nothing else owns.
    Ok(Dir(unsafe { OwnedFd::from_raw_fd(r) }))
}

/// The home folder of `uid` in the user database, if it has an entry there.
pub(crate) fn passwd_home(uid: u32) -> Option<PathBuf> {
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: an all-zero passwd is valid; getpwuid_r fills it in.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call; `buf` holds the strings `pwd` points to.
    let r = unsafe {
        let b = buf.as_mut_ptr().cast();
        libc::getpwuid_r(uid, &mut pwd, b, buf.len(), &mut found)
    };
    if r != 0 || found.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    // SAFETY: pw_dir points to a NUL-terminated string in `buf`, still alive here.
    let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
    Some(PathBuf::from(OsStr::from_bytes(dir.to_bytes())))
}

/// (used, total) bytes of the filesystem holding `path`.
pub(crate) fn fs_usage(path: &Path) -> io::Result<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: an all-zero statvfs is valid; statvfs fills it in.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `st` is writable.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let unit = st.f_frsize as u64;
    let total = st.f_blocks as u64 * unit;
    Ok((total - st.f_bfree as u64 * unit, total))
}

/// `secs` since the epoch as local time, `YYYY-MM-DDThh:mm:ss`.
pub(crate) fn local_datetime(secs: i64) -> String {
    let t = secs as libc::time_t;
    // SAFETY: an all-zero tm is valid; localtime_r fills it in.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TempDir;
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt};

    #[test]
    fn stat_agrees_with_std_and_does_not_follow_links() {
        let t = TempDir::new("statx");
        fs::create_dir(t.0.join("d")).unwrap();
        fs::write(t.0.join("d/f"), vec![1u8; 5000]).unwrap();
        symlink("/", t.0.join("to-root")).unwrap();
        let d = Dir::open(&t.0.join("d")).unwrap();
        let (own, f) = (d.stat().unwrap(), d.stat_at(c"f").unwrap());
        let md = fs::symlink_metadata(t.0.join("d/f")).unwrap();
        assert_eq!(
            (f.dev, f.ino, f.size, f.blocks),
            (md.dev(), md.ino(), 5000, md.blocks())
        );
        assert_eq!((own.kind, f.kind, own.mount_root), (DIR, FILE, false));
        assert!(own.mount_root_known, "this kernel reports mount roots");
        let link = stat_path(&t.0.join("to-root")).unwrap();
        assert_eq!((link.kind, link.mount_root), (SYMLINK, false));
        // With O_DIRECTORY, Linux answers a symlink with ENOTDIR rather than ELOOP.
        let err = Dir::open(&t.0.join("to-root")).err().unwrap();
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ENOTDIR),
            "a symlink is not opened"
        );
    }

    #[test]
    fn a_place_needs_only_search_permission_and_is_never_a_symlink() {
        let t = TempDir::new("place");
        fs::create_dir_all(t.0.join("x/inner")).unwrap();
        symlink("x", t.0.join("link")).unwrap();
        let top = Dir::open(&t.0).unwrap();
        let err = top.place_at(c"link").err().unwrap();
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ENOTDIR),
            "O_PATH does not open a symlink"
        );
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let x = t.0.join("x");
        fs::set_permissions(&x, fs::Permissions::from_mode(0o100)).unwrap(); // test-only
        let place = top.place_at(c"x").map(|d| d.place_at(c"inner").is_ok());
        let read = top.open_at(c"x").is_ok();
        fs::set_permissions(&x, fs::Permissions::from_mode(0o755)).unwrap(); // test-only
        assert_eq!(
            (place.ok(), read),
            (Some(true), false),
            "search only: a place, not a listing"
        );
    }

    #[test]
    fn passwd_home_is_a_full_path() {
        let home = passwd_home(unsafe { libc::geteuid() });
        assert!(home.is_none_or(|h| h.is_absolute()));
    }

    #[test]
    fn each_lists_every_name_once() {
        let t = TempDir::new("each");
        for i in 0..2000 {
            fs::write(t.0.join(format!("file-with-a-longish-name-{i}")), b"").unwrap();
        }
        let mut names = Vec::new();
        let d = Dir::open(&t.0).unwrap();
        d.each(&mut Vec::new(), |n| names.push(n.to_owned()))
            .unwrap();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 2000);
    }

    #[test]
    fn local_datetime_has_the_trash_spec_shape() {
        let s = local_datetime(0);
        assert_eq!(s.len(), 19);
        assert_eq!((&s[4..5], &s[10..11], &s[13..14]), ("-", "T", ":"));
    }
}
