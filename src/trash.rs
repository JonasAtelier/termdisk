// SPDX-License-Identifier: GPL-3.0-only

//! The freedesktop.org trash, the home one only (`$XDG_DATA_HOME/Trash`): an item moved there
//! can be put back from the file manager. The item is renamed in, so it must be on the trash's
//! filesystem; it is never copied. Only `guard::trash` gets here, with a `Checked` item. The
//! only deleting in termdisk is `empty`, of what is in this trash, reached only from the
//! guard's `ToEmpty`.

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::guard::Checked;
use crate::sys::{self, Dir};
use crate::tree::DIR;

/// The home trash's folders, open.
pub(crate) struct Trash {
    top: Dir,
    files: Dir,
    info: Dir,
    files_path: PathBuf,
    /// The device they are on: only an item on it can be moved in.
    pub dev: u64,
}

/// The home trash as `find` met it.
pub(crate) enum Found {
    Trash(Trash),
    /// Not all there yet: what is made goes on this device.
    Missing(u64),
}

impl Trash {
    /// Opens the home trash, making what is missing of it (see `find`).
    pub fn open(data_home: &Path, home: &Path, euid: u32) -> io::Result<Trash> {
        match find(data_home, home, euid, true)? {
            Found::Trash(t) => Ok(t),
            Found::Missing(_) => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }
}

/// Finds `data_home/Trash` and its `files` and `info`; with `make`, makes what is missing of
/// them, mode 0700 (the spec, and the XDG base directory spec for `data_home`'s own folders).
/// `data_home` is opened as given (a folder above it may be a symlink, as `/home` is on some
/// systems; it may not be one itself). Where part of it is missing, the deepest folder there is
/// must be `home` or inside it (`inside`), and the user's: the rest is made in it, one folder at
/// a time, none through a symlink. `Trash`, `files` and `info` must be real folders of the
/// user's, `files` and `info` no mount point (what is mounted there would be emptied).
pub(crate) fn find(data_home: &Path, home: &Path, euid: u32, make: bool) -> io::Result<Found> {
    let mut missing = Vec::new();
    let mut at = data_home;
    let mut dir = loop {
        match Dir::open(at) {
            Ok(d) => break d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let (Some(up), Some(name)) = (at.parent(), at.file_name()) else {
                    return Err(e);
                };
                missing.push(CString::new(name.as_bytes())?);
                at = up;
            }
            Err(e) => return Err(e),
        }
    };
    if !missing.is_empty() {
        let mine = dir.stat()?.uid == euid;
        if !mine || !inside(at, home) {
            return Err(io::Error::other(format!(
                "{} does not exist, and termdisk makes folders only inside your home",
                data_home.display()
            )));
        }
        if !make {
            return Ok(Found::Missing(dir.stat()?.dev));
        }
        for name in missing.iter().rev() {
            dir = folder(&dir, name, euid, true)?.ok_or(io::ErrorKind::NotFound)?;
        }
    }
    let Some(top) = folder(&dir, c"Trash", euid, make)? else {
        return Ok(Found::Missing(dir.stat()?.dev));
    };
    let (Some(files), Some(info)) = (
        folder(&top, c"files", euid, make)?,
        folder(&top, c"info", euid, make)?,
    ) else {
        return Ok(Found::Missing(top.stat()?.dev));
    };
    for (dir, name) in [(&files, "files"), (&info, "info")] {
        if dir.stat()?.mount_root {
            return Err(io::Error::other(format!(
                "the trash's {name} folder is a mount point"
            )));
        }
    }
    let dev = files.stat()?.dev;
    Ok(Found::Trash(Trash {
        top,
        files,
        info,
        files_path: data_home.join("Trash/files"),
        dev,
    }))
}

/// Whether `path` is `home` or inside it, as written: a path with a `..` in it is not (it may
/// lead anywhere), nor is anything when the home is `/`.
pub(crate) fn inside(path: &Path, home: &Path) -> bool {
    home.parent().is_some()
        && path.starts_with(home)
        && !path.components().any(|c| c == Component::ParentDir)
}

/// The folder `name` in `parent`, made first (mode 0700) with `make`; `None` if it is missing.
/// It must be a real folder (not a symlink) of `euid`'s.
fn folder(parent: &Dir, name: &CStr, euid: u32, make: bool) -> io::Result<Option<Dir>> {
    // SAFETY: `name` is NUL-terminated.
    if make && unsafe { libc::mkdirat(parent.fd(), name.as_ptr(), 0o700) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    let dir = match parent.open_at(name) {
        Err(e) if !make && e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r?, // O_NOFOLLOW: a symlink there is an error
    };
    if dir.stat()?.uid != euid {
        let name = name.to_string_lossy();
        return Err(io::Error::other(format!(
            "the trash's {name} folder is not yours"
        )));
    }
    Ok(Some(dir))
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
                // The info file made above, nothing else.
                let _ = unlink(&trash.info, &info_name, 0);
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

/// Removes `name` in `dir`: a folder (empty) with `AT_REMOVEDIR` in `flags`, else anything but
/// a folder (a symlink itself, never what it points to).
fn unlink(dir: &Dir, name: &CStr, flags: i32) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated.
    if unsafe { libc::unlinkat(dir.fd(), name.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

/// What emptying the trash has done so far, and a stop, shared with the thread that draws.
#[derive(Default)]
pub(crate) struct Tally {
    pub items: AtomicU64,
    pub bytes: AtomicU64,
    pub stop: AtomicBool,
}

/// What emptying the trash did.
#[derive(Debug, Default)]
pub(crate) struct Emptied {
    /// Items of the trash removed whole, and the bytes on disk that freed (with what their
    /// `.trashinfo` took).
    pub items: u64,
    pub bytes: u64,
    /// Items left (whole or in part), each with the first path in it that could not go, and
    /// why.
    pub failed: Vec<(PathBuf, String)>,
    /// Stopped before the end (the items not reached are still there).
    pub stopped: bool,
}

/// Deletes what is in the trash for good: each item in `files`, then its `.trashinfo` (the
/// spec's order: an item stopped half way still shows in the trash), then `directorysizes`.
/// `Trash`, `files` and `info` stay. Only through the folders held open here, none reached
/// through a symlink (a symlink in it is removed, not followed); a mount point, anything on
/// another filesystem and anything not `euid`'s stay where they are. Goes on past what fails.
/// `tally.stop` stops it at the next entry; the item it was in keeps its `.trashinfo`.
pub(crate) fn empty(trash: &Trash, euid: u32, tally: &Tally) -> Emptied {
    let mut out = Emptied::default();
    let mut buf = Vec::new();
    let mut names = Vec::new();
    if let Err(e) = trash.files.each(&mut buf, |n| names.push(n.to_owned())) {
        out.failed.push((trash.files_path.clone(), e.to_string()));
        return out;
    }
    let rules = (trash.dev, euid);
    for name in names {
        let path = trash.files_path.join(OsStr::from_bytes(name.to_bytes()));
        let mut why = None;
        if remove(&trash.files, &name, &path, rules, tally, &mut buf, &mut why) {
            // What could not go in it before the stop is still reported.
            out.failed.extend(why);
            out.stopped = true;
            break;
        }
        match why {
            Some(w) => out.failed.push(w),
            None => {
                let info = CString::new([name.to_bytes(), b".trashinfo"].concat());
                if let Ok(info) = info {
                    let st = trash.info.stat_at(&info);
                    if let (Ok(st), Ok(())) = (st, unlink(&trash.info, &info, 0)) {
                        tally.bytes.fetch_add(st.blocks * 512, Relaxed);
                    }
                }
                out.items += 1;
                tally.items.fetch_add(1, Relaxed);
            }
        }
    }
    let _ = unlink(&trash.top, c"directorysizes", 0); // a cache of the sizes in `files`
    out.bytes = tally.bytes.load(Relaxed);
    out
}

/// Removes `name` in `dir` (at `path`), a folder's entries first, each checked as it is met:
/// on the trash's device `rules.0`, not a mount point, `rules.1`'s. What cannot go stays; the
/// first one, and why, goes in `why`. Returns whether `tally.stop` stopped it (then what is
/// left of it stays).
fn remove(
    dir: &Dir,
    name: &CStr,
    path: &Path,
    rules: (u64, u32),
    tally: &Tally,
    buf: &mut Vec<u8>,
    why: &mut Option<(PathBuf, String)>,
) -> bool {
    if tally.stop.load(Relaxed) {
        return true;
    }
    let st = match dir.stat_at(name) {
        Ok(s) => s,
        Err(e) => return fail(why, path, e.to_string()),
    };
    if st.mount_root || st.dev != rules.0 {
        let e = "a mount point, or another filesystem: left as it is";
        return fail(why, path, e.into());
    }
    if st.uid != rules.1 {
        return fail(why, path, "not yours".into());
    }
    let flags = match st.kind {
        DIR => {
            let sub = match dir.open_at(name) {
                Ok(d) => d,
                Err(e) => return fail(why, path, e.to_string()),
            };
            match sub.stat() {
                Ok(s) if (s.dev, s.ino) == (st.dev, st.ino) => {}
                Ok(_) => return fail(why, path, "it changed while being emptied".into()),
                Err(e) => return fail(why, path, e.to_string()),
            }
            let mut names = Vec::new();
            if let Err(e) = sub.each(buf, |n| names.push(n.to_owned())) {
                return fail(why, path, e.to_string());
            }
            for n in names {
                let at = path.join(OsStr::from_bytes(n.to_bytes()));
                if remove(&sub, &n, &at, rules, tally, buf, why) {
                    return true;
                }
            }
            if why.is_some() {
                return false; // something in it stays, so it does
            }
            libc::AT_REMOVEDIR
        }
        _ => 0,
    };
    if let Err(e) = unlink(dir, name, flags) {
        return fail(why, path, e.to_string());
    }
    // A file with other hard links frees nothing yet.
    if st.kind == DIR || st.nlink <= 1 {
        tally.bytes.fetch_add(st.blocks * 512, Relaxed);
    }
    false
}

/// Keeps the first thing that could not go, and why; not stopped.
fn fail(why: &mut Option<(PathBuf, String)>, path: &Path, e: String) -> bool {
    why.get_or_insert_with(|| (path.to_path_buf(), e));
    false
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
