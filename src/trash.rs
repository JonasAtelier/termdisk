// SPDX-License-Identifier: GPL-3.0-only

//! The freedesktop.org trash, the home one only (`$XDG_DATA_HOME/Trash`): an item moved there
//! can be put back from the file manager. The item is renamed in, so it must be on the trash's
//! filesystem; it is never copied, and nothing of it is ever deleted. Only `guard::trash` gets
//! here, with a `Checked` item.

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::guard::Checked;
use crate::sys::{self, Dir};

/// The home trash's two folders, open.
pub(crate) struct Trash {
    files: Dir,
    info: Dir,
    files_path: PathBuf,
    /// The device they are on: only an item on it can be moved in.
    pub dev: u64,
}

impl Trash {
    /// Opens `data_home/Trash` and its `files` and `info`, making them (mode 0700) if absent.
    /// Each must be a real folder, not a symlink, owned by `euid`.
    pub fn open(data_home: &Path, euid: u32) -> io::Result<Trash> {
        let data = Dir::open(data_home)?;
        let top = folder(&data, c"Trash", euid)?;
        let files = folder(&top, c"files", euid)?;
        let info = folder(&top, c"info", euid)?;
        let dev = files.stat()?.dev;
        Ok(Trash {
            files,
            info,
            files_path: data_home.join("Trash/files"),
            dev,
        })
    }
}

fn folder(parent: &Dir, name: &CStr, euid: u32) -> io::Result<Dir> {
    // SAFETY: `name` is NUL-terminated.
    if unsafe { libc::mkdirat(parent.fd(), name.as_ptr(), 0o700) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    let dir = parent.open_at(name)?; // O_NOFOLLOW: a symlink there is an error
    if dir.stat()?.uid != euid {
        let name = name.to_string_lossy();
        return Err(io::Error::other(format!(
            "the trash's {name} folder is not yours"
        )));
    }
    Ok(dir)
}

/// Moves the item into the trash, as the spec says: its `.trashinfo` is made first (with
/// `O_EXCL`, which reserves the name), then the item is renamed in without replacing anything.
/// If that fails, the info file made here is removed and the item stays where it was.
pub(crate) fn put(trash: &Trash, item: Checked) -> io::Result<PathBuf> {
    let base = item.name.to_bytes();
    let info = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        escape(&item.path),
        sys::local_datetime(now())
    );
    // ponytail: a name of over 245 bytes leaves no room for ".trashinfo" and fails here;
    // shorten it in the trash if that ever shows up.
    for n in 1..=1000u32 {
        let name = match n {
            1 => base.to_vec(),
            _ => [base, format!(".{n}").as_bytes()].concat(),
        };
        let info_name = CString::new([&name[..], b".trashinfo"].concat())?;
        let file = match create(&trash.info, &info_name) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            r => r?,
        };
        let name = CString::new(name)?;
        let moved = File::from(file).write_all(info.as_bytes()).and_then(|()| {
            // A last look: still the inode every check was made on.
            let now = item.parent.stat_at(&item.name)?;
            if (now.dev, now.ino) != (item.stat.dev, item.stat.ino) {
                return Err(io::Error::other("it changed while being checked"));
            }
            rename_noreplace(&item.parent, &item.name, &trash.files, &name)
        });
        match moved {
            Ok(()) => return Ok(trash.files_path.join(OsStr::from_bytes(name.to_bytes()))),
            Err(e) => {
                unlink(&trash.info, &info_name); // the info file made above, nothing else
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
        }
    }
    Err(io::Error::other("no free name in the trash"))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A path as the spec's `Path=` wants it: bytes other than letters, digits, `-_.~/` as `%XX`.
fn escape(path: &Path) -> String {
    let mut s = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn create(dir: &Dir, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated.
    let r = unsafe { libc::openat(dir.fd(), name.as_ptr(), flags, 0o600 as libc::c_uint) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `r` is a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(r) })
}

fn unlink(dir: &Dir, name: &CStr) {
    // SAFETY: `name` is NUL-terminated.
    unsafe { libc::unlinkat(dir.fd(), name.as_ptr(), 0) };
}

/// The only rename in termdisk: `name` in `from` becomes `new` in `to`, unless `new` exists.
fn rename_noreplace(from: &Dir, name: &CStr, to: &Dir, new: &CStr) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and both descriptors are open folders.
    let r = unsafe {
        let (old, new) = (name.as_ptr(), new.as_ptr());
        let flags = libc::RENAME_NOREPLACE;
        libc::syscall(libc::SYS_renameat2, from.fd(), old, to.fd(), new, flags)
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::escape;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[test]
    fn escape_keeps_the_path_readable_and_exact() {
        let p = Path::new(OsStr::from_bytes(b"/home/me/a b%c/\xe9t\xc3\xa9.txt"));
        assert_eq!(escape(p), "/home/me/a%20b%25c/%E9t%C3%A9.txt");
    }
}
