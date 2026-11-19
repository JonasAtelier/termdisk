//! The one way termdisk removes anything: `trash`, which moves one item to the home trash.
//! Before it does, it checks on the disk itself, at that moment, never trusting the scan:
//!
//! 1. nothing is ever deleted: the item is renamed into the trash, or left where it is;
//! 2. no extra powers: a process that is root, runs setuid, or may change others' files
//!    cannot trash;
//! 3. the item and everything under it belong to the user;
//! 4. it is not, does not hold and does not lie inside a protected path (`Policy`: the lists
//!    below, applied to `$HOME` and to the user's home in the passwd database);
//! 5. no mount point is in or under it, and every folder in it can be read;
//! 6. no symlink is followed: its parent is reached from `/` one folder at a time (`O_PATH`, so
//!    search permission is enough), the item must still be the inode the scan saw, and the
//!    check and the rename use that same parent;
//! 9. not the scan root, not trashed already (and a tree only exists once its scan is done);
//! 10. the trash folders are real folders of the user's, on the item's filesystem;
//! 11. when unsure (no HOME, unreadable capabilities, an old kernel), nothing can be trashed;
//! 12. `trash` is the only caller of `trash::put`, which needs a `Checked` made only here; and
//!     `Policy::detect` is the only place that reads HOME, XDG_* and the passwd entry, so a
//!     program embedding termdisk cannot hand it a home of its own.

use std::collections::HashSet;
use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::sys::{self, Dir, Stat};
use crate::trash::{put, Trash};
use crate::tree::{NodeId, Tree, DIR, TRASHED};

/// Home paths protected with everything under them: settings and app data (browsers, mail,
/// messengers, containers, flatpak and snap apps), keys and credentials, shell setup and
/// history, and folders a sync client mirrors (trashing there deletes in the cloud too).
const HOME_TREES: &[&str] = &[
    ".config",
    ".local",
    ".var",
    "snap",
    ".mozilla",
    ".thunderbird",
    ".ssh",
    ".gnupg",
    ".pki",
    ".password-store",
    ".aws",
    ".kube",
    ".docker",
    ".netrc",
    ".git-credentials",
    ".gitconfig",
    ".cargo/credentials.toml",
    ".npmrc",
    ".pypirc",
    ".claude",
    ".claude.json",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".bash_history",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zsh_history",
    ".histfile",
    ".python_history",
    ".Xauthority",
    "Dropbox",
    "OneDrive",
    "Nextcloud",
    "ownCloud",
    "pCloudDrive",
    "MEGA",
    "Sync",
];

/// Home folders protected themselves only: what is in them may go (`~/.cache/app` may,
/// `~/.cache` may not).
const HOME_FOLDERS: &[&str] = &[".cache"];

/// The XDG user folders: their key in `user-dirs.dirs`, their usual name, and whether
/// everything in them is protected too (Downloads and Videos are where space goes; their
/// contents may be trashed).
const USER_DIRS: &[(&str, &str, bool)] = &[
    ("DESKTOP", "Desktop", true),
    ("DOCUMENTS", "Documents", true),
    ("PICTURES", "Pictures", true),
    ("MUSIC", "Music", true),
    ("DOWNLOAD", "Downloads", false),
    ("VIDEOS", "Videos", false),
    ("TEMPLATES", "Templates", false),
    ("PUBLICSHARE", "Public", false),
];

/// What may be trashed, worked out once per session.
pub struct Policy {
    pub(crate) euid: u32,
    /// The home the start screen offers (the first of the homes protected).
    home: PathBuf,
    cache_home: PathBuf,
    data_home: PathBuf,
    /// Protected paths; `true` protects everything under the path too.
    protected: Vec<(PathBuf, bool)>,
    /// Why nothing may be trashed at all, if so.
    read_only: Option<String>,
}

impl Policy {
    /// The policy for this process: `read_only` from `--read-only`; HOME, XDG_DATA_HOME,
    /// XDG_CONFIG_HOME, XDG_CACHE_HOME and XDG_RUNTIME_DIR from the environment, and the
    /// user's home from the passwd database. The only place termdisk reads any of these.
    pub fn detect(read_only: bool) -> Policy {
        // SAFETY: getuid and geteuid cannot fail.
        let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
        let status = fs::read_to_string("/proc/self/status").ok();
        let passwd = sys::passwd_home(euid);
        Policy::from_env(
            read_only,
            uid,
            euid,
            |k| env::var_os(k),
            passwd,
            status.as_deref(),
        )
    }

    /// `detect`, given the environment (`var`), the passwd home and `/proc/self/status`.
    fn from_env(
        read_only: bool,
        uid: u32,
        euid: u32,
        var: impl Fn(&str) -> Option<OsString>,
        passwd_home: Option<PathBuf>,
        status: Option<&str>,
    ) -> Policy {
        let var = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
        let canon = |p: PathBuf| fs::canonicalize(&p).unwrap_or(p);
        let real_dir = |p: PathBuf| fs::canonicalize(p).ok().filter(|p| p.is_dir());
        let raw_home = var("HOME");
        let home = raw_home.clone().and_then(real_dir);
        let home_stat = home.as_deref().and_then(|h| sys::stat_path(h).ok());
        let mount_roots = home_stat.is_some_and(|s| s.mount_root_known);
        let owner = home_stat.map(|s| s.uid);
        let why = read_only_reason(read_only, uid, euid, status, owner, mount_roots);
        // Both homes get every list: HOME may be set to anything, the passwd entry may be absent.
        let mut homes: Vec<PathBuf> = home.into_iter().collect();
        if let Some(p) = passwd_home
            .and_then(real_dir)
            .filter(|p| !homes.contains(p))
        {
            homes.push(p);
        }
        let first = homes.first().cloned().unwrap_or_else(|| PathBuf::from("/"));
        let data = canon(var("XDG_DATA_HOME").unwrap_or_else(|| first.join(".local/share")));
        let config = canon(var("XDG_CONFIG_HOME").unwrap_or_else(|| first.join(".config")));
        let runtime = var("XDG_RUNTIME_DIR").map(canon);
        let raw = raw_home.as_deref();
        let mut policy = Policy::new(euid, raw, &homes, &data, &config, runtime.as_deref(), why);
        if let Some(cache) = var("XDG_CACHE_HOME").map(canon) {
            policy.cache_home = cache;
        }
        policy
    }

    /// The protected paths: `/`; `raw_home` itself (HOME as set, maybe a symlink); the data,
    /// config and runtime folders whole; and for each of `homes` the home itself, the lists
    /// above, the XDG user folders (named in `user-dirs.dirs`, plus their usual names), and
    /// the paths listed in a `termdisk/protect` file (one per line, absolute or `~/...`; `#`
    /// starts a comment). That file can only add.
    pub(crate) fn new(
        euid: u32,
        raw_home: Option<&Path>,
        homes: &[PathBuf],
        data_home: &Path,
        config_home: &Path,
        runtime: Option<&Path>,
        read_only: Option<String>,
    ) -> Policy {
        let mut protected = Vec::new();
        let mut add = |path: PathBuf, tree: bool| {
            if let Ok(real) = fs::canonicalize(&path) {
                if real != path {
                    protected.push((real, tree));
                }
            }
            protected.push((path, tree));
        };
        add(PathBuf::from("/"), false);
        if let Some(raw) = raw_home {
            add(raw.to_path_buf(), false);
        }
        add(data_home.to_path_buf(), true);
        add(config_home.to_path_buf(), true);
        if let Some(r) = runtime {
            add(r.to_path_buf(), true);
        }
        let read = |p: PathBuf| fs::read_to_string(p).unwrap_or_default();
        for home in homes {
            add(home.clone(), false);
            for p in HOME_TREES {
                add(home.join(p), true);
            }
            for p in HOME_FOLDERS {
                add(home.join(p), false);
            }
            for &(_, name, tree) in USER_DIRS {
                add(home.join(name), tree);
            }
            for config in [config_home.to_path_buf(), home.join(".config")] {
                for (p, tree) in user_dirs_in(&read(config.join("user-dirs.dirs")), home) {
                    add(p, tree);
                }
                for p in paths_in(&read(config.join("termdisk/protect")), home) {
                    add(p, true);
                }
            }
        }
        let home = homes.first().cloned().unwrap_or_else(|| PathBuf::from("/"));
        Policy {
            euid,
            cache_home: home.join(".cache"),
            home,
            data_home: data_home.to_path_buf(),
            protected,
            read_only,
        }
    }

    /// Why nothing may be trashed, if so.
    pub fn read_only(&self) -> Option<&str> {
        self.read_only.as_deref()
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn cache_home(&self) -> &Path {
        &self.cache_home
    }

    /// The home trash folder (it may not exist yet).
    pub fn trash_dir(&self) -> PathBuf {
        self.data_home.join("Trash")
    }

    /// Whether items on `dev` could be renamed into the home trash at all.
    pub fn trash_reaches(&self, dev: u64) -> bool {
        sys::stat_path(&self.data_home).is_ok_and(|s| s.dev == dev)
    }

    /// Marks the protected paths in a scanned tree for the view. Only a hint: `trash` checks
    /// everything again.
    pub fn mark(&self, tree: &mut Tree) {
        let root = tree.root_path().to_path_buf();
        for (p, subtree) in &self.protected {
            if let Some(id) = p.strip_prefix(&root).ok().and_then(|rel| tree.find(rel)) {
                tree.mark_protected(id, *subtree);
            }
        }
    }

    /// The protected path that `path` is, holds, or lies inside (when that one's whole tree is
    /// protected); the scan root counts as protected too.
    fn protected_by(&self, path: &Path, root: &Path) -> Option<PathBuf> {
        let root = (root.to_path_buf(), false);
        let found = self
            .protected
            .iter()
            .chain([&root])
            .find(|(p, tree)| p.starts_with(path) || (*tree && path.starts_with(p)));
        found.map(|(p, _)| p.clone())
    }

    /// The protected paths' (device, inode) as they are now, to spot them under another name.
    fn protected_inodes(&self) -> HashSet<(u64, u64)> {
        self.protected
            .iter()
            .filter_map(|(p, _)| sys::stat_path(p).ok())
            .map(|s| (s.dev, s.ino))
            .collect()
    }
}

/// Why this process may not trash anything, if so (rules 2 and 11). `status` is
/// `/proc/self/status`; `home_owner`, who owns the real HOME folder (`None`: no such folder);
/// `mount_roots`, whether the kernel reports mount roots.
fn read_only_reason(
    flag: bool,
    uid: u32,
    euid: u32,
    status: Option<&str>,
    home_owner: Option<u32>,
    mount_roots: bool,
) -> Option<String> {
    // CAP_DAC_OVERRIDE (1) and CAP_FOWNER (3) let a process change files that are not its own.
    let caps = status
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("CapEff:")))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok());
    let why = if flag {
        "read-only (--read-only)"
    } else if euid == 0 {
        "read-only: running as root"
    } else if uid != euid {
        "read-only: running setuid (as another user than the one who started it)"
    } else if caps.is_none() {
        "read-only: cannot read this process's capabilities"
    } else if caps.is_some_and(|c| c & (1 << 1 | 1 << 3) != 0) {
        "read-only: running with the power to change other users' files"
    } else if home_owner.is_none() {
        "read-only: HOME is not set to a folder"
    } else if home_owner != Some(euid) {
        "read-only: HOME is not your folder"
    } else if !mount_roots {
        "read-only: this kernel does not report mount points (Linux 5.8 or later needed)"
    } else {
        return None;
    };
    Some(why.to_string())
}

/// The folders `user-dirs.dirs` names (`XDG_DOWNLOAD_DIR="$HOME/Downloads"`), but not HOME,
/// each with whether its whole tree is protected (see `USER_DIRS`; other keys: itself only).
fn user_dirs_in(text: &str, home: &Path) -> Vec<(PathBuf, bool)> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("XDG_")?.split_once("_DIR="))
        .filter_map(|(key, v)| {
            let v = v.trim().trim_matches('"');
            let path = match v.strip_prefix("$HOME/") {
                Some(rest) if !rest.is_empty() => Some(home.join(rest)),
                _ => Some(PathBuf::from(v)).filter(|p| p.is_absolute() && p != Path::new("/")),
            }?;
            let tree = USER_DIRS.iter().any(|&(k, _, tree)| k == key && tree);
            Some((path, tree))
        })
        .collect()
}

/// The paths a protect file lists: absolute or `~/...`, one per line; anything else is skipped.
fn paths_in(text: &str, home: &Path) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| match l.strip_prefix("~/") {
            Some(rest) => Some(home.join(rest)),
            None if l == "~" => Some(home.to_path_buf()),
            None => Some(PathBuf::from(l)).filter(|p| p.is_absolute()),
        })
        .collect()
}

/// Why `trash` did not move an item. Nothing was moved in every case.
#[derive(Debug)]
pub enum Refusal {
    ReadOnly(String),
    /// The folder the scan started from.
    ScanRoot,
    AlreadyTrashed,
    /// It is, holds, or lies inside this protected path.
    Protected(PathBuf),
    NotOwned(PathBuf),
    /// This could not be read, so the item could not be checked.
    Unreadable(PathBuf),
    /// A mount point, or another filesystem met at this path.
    Mount(PathBuf),
    /// Not on the filesystem of the home trash, so it cannot be renamed into it.
    OtherFilesystem(PathBuf),
    /// Not what the scan saw any more: scan again.
    Changed(PathBuf),
    /// Too many folders deep to check with the descriptors this process may open.
    TooDeep(PathBuf),
    /// The home trash folder cannot be used.
    NoTrash(String),
    /// The move itself failed.
    Failed(io::Error),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Refusal::ReadOnly(why) => write!(f, "{why}"),
            Refusal::ScanRoot => write!(f, "the scanned folder itself cannot be trashed"),
            Refusal::AlreadyTrashed => write!(f, "already in the trash"),
            Refusal::Protected(p) => write!(f, "protected: {}", p.display()),
            Refusal::NotOwned(p) => write!(f, "not yours: {}", p.display()),
            Refusal::Unreadable(p) => write!(f, "cannot read {}, so cannot check it", p.display()),
            Refusal::Mount(p) => write!(f, "a mount point: {}", p.display()),
            Refusal::OtherFilesystem(p) => write!(
                f,
                "{} is on another filesystem than the trash; nothing was moved",
                p.display()
            ),
            Refusal::Changed(p) => write!(f, "{} changed since the scan; scan again", p.display()),
            Refusal::TooDeep(p) => write!(
                f,
                "{} is nested too deep to check (too many folders open)",
                p.display()
            ),
            Refusal::NoTrash(e) => write!(f, "the trash cannot be used: {e}"),
            Refusal::Failed(e) => write!(f, "moving to the trash failed, nothing was moved: {e}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// An item moved to the trash.
#[derive(Debug)]
pub struct Trashed {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// An item that passed every check moments ago, held by its open parent. Only `checked` can
/// make one (the private field), and `trash::put` takes nothing else.
pub(crate) struct Checked {
    pub parent: Dir,
    pub name: CString,
    pub stat: Stat,
    pub path: PathBuf,
    _only_the_guard: (),
}

/// Moves `id` to the home trash if every rule above allows it. This frees no space until the
/// trash is emptied. It only reads the tree, so it can run away from the thread that draws it;
/// after a success the caller marks the item with `Tree::mark_trashed`.
///
/// It asks nothing: the confirmation (the full path, Cancel first, the delays: rule 7) is the
/// view's. A program that calls this directly instead of going through `View` must ask the
/// user itself.
pub fn trash(tree: &Tree, id: NodeId, policy: &Policy) -> Result<Trashed, Refusal> {
    let (item, trash) = checked(tree, id, policy)?;
    let from = item.path.clone();
    let to = put(&trash, item).map_err(Refusal::Failed)?;
    Ok(Trashed { from, to })
}

/// Every check `trash` makes, moving nothing: whether `trash` would go ahead right now (it
/// checks again when asked). It makes the trash folders if they are missing.
pub fn check(tree: &Tree, id: NodeId, policy: &Policy) -> Result<(), Refusal> {
    checked(tree, id, policy).map(|_| ())
}

fn checked(tree: &Tree, id: NodeId, policy: &Policy) -> Result<(Checked, Trash), Refusal> {
    if let Some(why) = &policy.read_only {
        return Err(Refusal::ReadOnly(why.clone()));
    }
    if id == tree.root() {
        return Err(Refusal::ScanRoot);
    }
    let node = *tree.node(id);
    if node.flags & TRASHED != 0 {
        return Err(Refusal::AlreadyTrashed);
    }
    let path = tree.path(id);
    if let Some(p) = policy.protected_by(&path, tree.root_path()) {
        return Err(Refusal::Protected(p));
    }
    let (Some(parent_path), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(Refusal::ScanRoot);
    };
    let parent = open_from_root(parent_path).map_err(|e| lost(e, parent_path))?;
    let name = CString::new(name.as_bytes()).map_err(|_| Refusal::Changed(path.clone()))?;
    let st = parent.stat_at(&name).map_err(|e| lost(e, &path))?;
    if st.ino != node.ino || st.kind != node.kind() {
        return Err(Refusal::Changed(path));
    }
    if st.mount_root || st.dev != tree.dev {
        return Err(Refusal::Mount(path));
    }
    if st.uid != policy.euid {
        return Err(Refusal::NotOwned(path));
    }
    let protected = policy.protected_inodes();
    if protected.contains(&(st.dev, st.ino)) {
        return Err(Refusal::Protected(path));
    }
    let trash =
        Trash::open(&policy.data_home, policy.euid).map_err(|e| Refusal::NoTrash(e.to_string()))?;
    if st.dev != trash.dev {
        return Err(Refusal::OtherFilesystem(path));
    }
    if st.kind == DIR {
        let dir = parent.open_at(&name).map_err(|e| lost(e, &path))?;
        let own = dir.stat().map_err(|e| lost(e, &path))?;
        if (own.dev, own.ino) != (st.dev, st.ino) {
            return Err(Refusal::Changed(path));
        }
        let rules = Rules {
            dev: st.dev,
            euid: policy.euid,
            protected,
        };
        check_tree(&dir, &path, &rules, &mut Vec::new())?;
    }
    let item = Checked {
        parent,
        name,
        stat: st,
        path,
        _only_the_guard: (),
    };
    Ok((item, trash))
}

/// What every entry under a folder being trashed must meet.
struct Rules {
    dev: u64,
    euid: u32,
    protected: HashSet<(u64, u64)>,
}

/// Checks everything under `dir` (at `path`), one open folder at a time.
fn check_tree(dir: &Dir, path: &Path, rules: &Rules, buf: &mut Vec<u8>) -> Result<(), Refusal> {
    let mut refusal = None;
    let mut subdirs = Vec::new();
    let listed = dir.each(buf, |name| {
        if refusal.is_some() {
            return;
        }
        let at = || path.join(OsStr::from_bytes(name.to_bytes()));
        refusal = match dir.stat_at(name) {
            Err(_) => Some(Refusal::Unreadable(at())),
            Ok(s) if s.mount_root || s.dev != rules.dev => Some(Refusal::Mount(at())),
            Ok(s) if s.uid != rules.euid => Some(Refusal::NotOwned(at())),
            Ok(s) if rules.protected.contains(&(s.dev, s.ino)) => Some(Refusal::Protected(at())),
            Ok(s) => {
                if s.kind == DIR {
                    subdirs.push((name.to_owned(), s.ino));
                }
                None
            }
        };
    });
    listed.map_err(|_| Refusal::Unreadable(path.to_path_buf()))?;
    if let Some(r) = refusal {
        return Err(r);
    }
    for (name, ino) in subdirs {
        let at = path.join(OsStr::from_bytes(name.to_bytes()));
        let sub = dir.open_at(&name).map_err(|e| lost(e, &at))?;
        if sub.stat().map_err(|e| lost(e, &at))?.ino != ino {
            return Err(Refusal::Changed(at));
        }
        check_tree(&sub, &at, rules, buf)?;
    }
    Ok(())
}

/// Reaches the folder at the absolute `path` from `/`, one component at a time, none through
/// a symlink, each opened as a place only (search permission is enough).
fn open_from_root(path: &Path) -> io::Result<Dir> {
    let mut dir = Dir::root_place()?;
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(name) => dir = dir.place_at(&CString::new(name.as_bytes())?)?,
            _ => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
        }
    }
    Ok(dir)
}

/// The refusal for not reaching `path`: unreadable if access was denied, too deep if out of
/// descriptors, else changed.
fn lost(e: io::Error, path: &Path) -> Refusal {
    let path = path.to_path_buf();
    match e.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => Refusal::Unreadable(path),
        Some(libc::EMFILE | libc::ENFILE) => Refusal::TooDeep(path),
        _ => Refusal::Changed(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{PROTECTED, SYMLINK};
    use crate::TempDir;
    use std::fs::Permissions;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::process::Command;

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    /// A temp folder holding a home (with a cache, keys, a protect file and a Downloads user
    /// folder), a runtime folder and an outside folder, and the policy for that home.
    struct Fix {
        t: TempDir,
        home: PathBuf,
        policy: Policy,
    }

    fn fix(tag: &str) -> Fix {
        let t = TempDir::new(tag);
        let home = fs::canonicalize(&t.0).unwrap().join("home");
        for d in [
            ".local/share",
            ".config/termdisk",
            ".cache/app/deep",
            ".ssh",
            "Downloads",
            "keep",
            "work/sub",
        ] {
            fs::create_dir_all(home.join(d)).unwrap();
        }
        fs::create_dir_all(t.0.join("run")).unwrap();
        fs::create_dir_all(t.0.join("outside/inner")).unwrap();
        fs::write(t.0.join("outside/inner/f"), b"outside").unwrap();
        fs::write(home.join(".cache/app/blob"), vec![7u8; 40_000]).unwrap();
        fs::write(home.join(".cache/app/deep/x"), b"x").unwrap();
        fs::write(home.join(".ssh/id"), b"key").unwrap();
        fs::write(home.join(".bashrc"), b"# rc").unwrap();
        fs::write(home.join("Downloads/f"), b"f").unwrap();
        fs::write(home.join("keep/f"), b"f").unwrap();
        fs::write(home.join("work/sub/x"), b"x").unwrap();
        let user_dirs = "# from xdg-user-dirs\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\nXDG_DESKTOP_DIR=\"$HOME/\"\n";
        fs::write(home.join(".config/user-dirs.dirs"), user_dirs).unwrap();
        let protect = "~/keep\n# a note\nrelative/is/skipped\n!~/.ssh\n";
        fs::write(home.join(".config/termdisk/protect"), protect).unwrap();
        let policy = policy_for(&home, &t.0, None);
        Fix { t, home, policy }
    }

    fn policy_for(home: &Path, t: &Path, read_only: Option<String>) -> Policy {
        let run = fs::canonicalize(t.join("run")).unwrap();
        let (data, config) = (home.join(".local/share"), home.join(".config"));
        let homes = [home.to_path_buf()];
        Policy::new(me(), None, &homes, &data, &config, Some(&run), read_only)
    }

    const NO_CAPS: &str = "Name:\ttermdisk\nCapEff:\t0000000000000000\n";

    fn scan(root: &Path) -> Tree {
        crate::scan(root, me()).unwrap().join().unwrap()
    }

    /// `trash`, then the mark a view makes after a success.
    fn try_trash(tree: &mut Tree, path: &Path, policy: &Policy) -> Result<Trashed, Refusal> {
        let id = tree
            .find(path)
            .unwrap_or_else(|| panic!("{} not in the tree", path.display()));
        let r = trash(tree, id, policy);
        if r.is_ok() {
            tree.mark_trashed(id);
        }
        r
    }

    #[test]
    fn rule1_trashing_moves_the_item_and_writes_its_info() {
        let f = fix("r1-move");
        let odd = f.home.join(".cache/app/a b%c");
        fs::write(&odd, b"odd").unwrap();
        let mut tree = scan(&f.t.0);
        let before = tree.node(0).alloc;
        let blob = f.home.join(".cache/app/blob");
        let done = try_trash(&mut tree, &blob, &f.policy).unwrap();
        let trash = f.home.join(".local/share/Trash");
        assert_eq!(
            (done.from.as_path(), done.to.clone()),
            (blob.as_path(), trash.join("files/blob"))
        );
        assert!(!blob.exists());
        assert_eq!(
            fs::read(trash.join("files/blob")).unwrap(),
            vec![7u8; 40_000]
        );
        let info = fs::read_to_string(trash.join("info/blob.trashinfo")).unwrap();
        let lines: Vec<&str> = info.lines().collect();
        assert_eq!(lines[0], "[Trash Info]");
        assert_eq!(lines[1], format!("Path={}", blob.display()));
        assert!(
            lines[2].starts_with("DeletionDate=") && lines[2].len() == 32,
            "{}",
            lines[2]
        );
        for d in ["", "/files", "/info"] {
            let mode = fs::metadata(format!("{}{d}", trash.display()))
                .unwrap()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "made private: Trash{d}");
        }
        let id = tree.find(&blob).unwrap();
        assert!(tree.node(id).flags & TRASHED != 0);
        assert!(tree.node(0).alloc < before);
        try_trash(&mut tree, &odd, &f.policy).unwrap();
        let info = fs::read_to_string(trash.join("info/a b%c.trashinfo")).unwrap();
        assert!(info.contains("/.cache/app/a%20b%25c\n"), "{info}");
    }

    #[test]
    fn rule1_a_second_item_of_the_same_name_gets_a_suffix() {
        let f = fix("r1-names");
        let mut tree = scan(&f.t.0);
        try_trash(&mut tree, &f.home.join(".cache/app/deep/x"), &f.policy).unwrap();
        let second = try_trash(&mut tree, &f.home.join("work/sub/x"), &f.policy).unwrap();
        let trash = f.home.join(".local/share/Trash");
        assert_eq!(second.to, trash.join("files/x.2"));
        assert!(
            trash.join("info/x.trashinfo").exists() && trash.join("info/x.2.trashinfo").exists()
        );
    }

    #[test]
    fn rule1_an_orphan_in_the_trash_is_never_replaced() {
        let f = fix("r1-orphan");
        Trash::open(&f.home.join(".local/share"), me())
            .ok()
            .unwrap();
        let trash = f.home.join(".local/share/Trash");
        fs::write(trash.join("files/blob"), b"orphan").unwrap(); // no info file
        let mut tree = scan(&f.t.0);
        let done = try_trash(&mut tree, &f.home.join(".cache/app/blob"), &f.policy).unwrap();
        assert_eq!(done.to, trash.join("files/blob.2"));
        assert_eq!(fs::read(trash.join("files/blob")).unwrap(), b"orphan");
        assert!(trash.join("info/blob.2.trashinfo").exists());
        assert!(!trash.join("info/blob.trashinfo").exists());
    }

    #[test]
    fn rule1_a_failed_move_leaves_the_item_and_no_info_file() {
        if me() == 0 {
            return;
        }
        let f = fix("r1-fail");
        let mut tree = scan(&f.t.0);
        Trash::open(&f.home.join(".local/share"), me())
            .ok()
            .unwrap();
        let files = f.home.join(".local/share/Trash/files");
        fs::set_permissions(&files, Permissions::from_mode(0o500)).unwrap(); // test-only
        let blob = f.home.join(".cache/app/blob");
        let r = try_trash(&mut tree, &blob, &f.policy);
        fs::set_permissions(&files, Permissions::from_mode(0o700)).unwrap(); // test-only
        assert!(matches!(r, Err(Refusal::Failed(_))), "{r:?}");
        assert!(blob.exists());
        let info = f.home.join(".local/share/Trash/info");
        assert_eq!(
            fs::read_dir(info).unwrap().count(),
            0,
            "the info file was taken back"
        );
        assert!(tree.node(tree.find(&blob).unwrap()).flags & TRASHED == 0);
    }

    #[test]
    fn rule2_and_11_read_only_when_privileged_or_unsure() {
        let (ok, mine) = (Some(NO_CAPS), Some(1000));
        assert_eq!(read_only_reason(false, 1000, 1000, ok, mine, true), None);
        let why = |flag, euid, status, home, mounts| {
            read_only_reason(flag, euid, euid, status, home, mounts).unwrap()
        };
        assert!(why(true, 1000, ok, mine, true).contains("--read-only"));
        assert!(why(false, 0, ok, Some(0), true).contains("root"));
        let setuid = read_only_reason(false, 1000, 1001, ok, mine, true).unwrap();
        assert!(setuid.contains("setuid"));
        let caps = |c| why(false, 1000, Some(c), mine, true);
        assert!(caps("CapEff:\t0000000000000002\n").contains("power"));
        assert!(caps("CapEff:\t0000000000000008\n").contains("power"));
        assert!(why(false, 1000, None, mine, true).contains("capabilities"));
        assert!(why(false, 1000, ok, None, true).contains("HOME is not set"));
        assert!(why(false, 1000, ok, Some(0), true).contains("HOME is not your"));
        assert!(why(false, 1000, ok, mine, false).contains("5.8"));
        let p = Path::new("/x");
        let emfile = io::Error::from_raw_os_error(libc::EMFILE);
        assert!(
            matches!(lost(emfile, p), Refusal::TooDeep(_)),
            "out of descriptors"
        );
        let f = fix("r2-refuse");
        let mut tree = scan(&f.t.0);
        let ro = policy_for(&f.home, &f.t.0, Some("read-only: test".into()));
        let r = try_trash(&mut tree, &f.home.join(".cache/app/blob"), &ro);
        assert!(matches!(r, Err(Refusal::ReadOnly(_))), "{r:?}");
    }

    #[test]
    fn rule2_this_process_may_trash() {
        // The developer's own session: not root, no extra capabilities, a HOME, Linux >= 5.8.
        if me() != 0 {
            assert_eq!(Policy::detect(false).read_only(), None);
        }
        assert!(Policy::detect(true).read_only().is_some());
    }

    #[test]
    fn rule3_items_and_everything_under_them_must_be_the_users() {
        let f = fix("r3-owner");
        let mut tree = scan(&f.t.0);
        let other = Policy {
            euid: me() + 1,
            ..policy_for(&f.home, &f.t.0, None)
        };
        let r = try_trash(&mut tree, &f.home.join("work"), &other);
        assert!(
            matches!(&r, Err(Refusal::NotOwned(p)) if p.ends_with("work")),
            "{r:?}"
        );
        // The check under a folder reads every entry's owner itself.
        let dir = Dir::open(&f.home.join("work")).unwrap();
        let rules = Rules {
            dev: dir.stat().unwrap().dev,
            euid: me() + 1,
            protected: HashSet::new(),
        };
        let r = check_tree(&dir, &f.home.join("work"), &rules, &mut Vec::new());
        assert!(
            matches!(&r, Err(Refusal::NotOwned(p)) if p.ends_with("work/sub")),
            "{r:?}"
        );
    }

    #[test]
    fn rule4_protected_paths_are_refused_and_what_is_in_cache_is_not() {
        let f = fix("r4-protected");
        Trash::open(&f.home.join(".local/share"), me())
            .ok()
            .unwrap();
        fs::write(f.home.join(".local/share/Trash/files/old"), b"old").unwrap();
        let mut tree = scan(&f.t.0);
        let root = tree.root_path().to_path_buf();
        assert!(matches!(
            try_trash(&mut tree, &root, &f.policy),
            Err(Refusal::ScanRoot)
        ));
        for p in [
            "home",
            "home/.ssh",
            "home/.ssh/id",
            "home/.bashrc",
            "home/.cache",
            "home/.config",
            "home/.config/termdisk",
            "home/.config/user-dirs.dirs",
            "home/.local",
            "home/.local/share/Trash",
            "home/.local/share/Trash/files/old",
            "home/Downloads",
            "home/keep",
            "home/keep/f",
            "run",
        ] {
            let r = try_trash(&mut tree, &root.join(p), &f.policy);
            assert!(matches!(r, Err(Refusal::Protected(_))), "{p}: {r:?}");
        }
        for p in ["home/.cache/app", "home/Downloads/f", "home/work"] {
            try_trash(&mut tree, &root.join(p), &f.policy).unwrap_or_else(|e| panic!("{p}: {e}"));
        }
    }

    #[test]
    fn rule4_every_listed_home_path_is_refused_and_the_line_is_where_it_says() {
        let t = TempDir::new("r4-list");
        let home = fs::canonicalize(&t.0).unwrap().join("home");
        // Written out, not taken from the lists: weakening a list must fail here.
        let whole = [
            ".config",
            ".config/google-chrome/Default",
            ".config/Signal",
            ".config/gh",
            ".config/termdisk",
            ".local",
            ".local/share/containers",
            ".local/share/evolution",
            ".local/share/keyrings",
            ".local/share/Trash",
            ".local/bin",
            ".local/state",
            ".var",
            ".var/app/org.example.App/data",
            "snap",
            "snap/firefox",
            ".mozilla",
            ".thunderbird",
            ".ssh",
            ".gnupg",
            ".pki",
            ".password-store",
            ".aws",
            ".kube",
            ".docker",
            ".netrc",
            ".git-credentials",
            ".gitconfig",
            ".cargo/credentials.toml",
            ".npmrc",
            ".pypirc",
            ".claude",
            ".claude.json",
            ".bashrc",
            ".bash_profile",
            ".bash_login",
            ".bash_logout",
            ".bash_history",
            ".profile",
            ".zshrc",
            ".zshenv",
            ".zprofile",
            ".zsh_history",
            ".histfile",
            ".python_history",
            ".Xauthority",
            "Dropbox",
            "OneDrive",
            "Nextcloud",
            "ownCloud",
            "pCloudDrive",
            "MEGA",
            "Sync",
            "Desktop",
            "Documents",
            "Pictures",
            "Music",
        ];
        let only = [".cache", "Downloads", "Videos", "Templates", "Public"];
        let free = [
            ".npm/_cacache",
            ".cargo/registry",
            ".rustup/toolchains",
            "src/target",
        ];
        for p in whole.iter().chain(&only).chain(&free) {
            fs::create_dir_all(home.join(p).join("inner")).unwrap();
        }
        let (data, config) = (home.join(".local/share"), home.join(".config"));
        let homes = [home.clone()];
        let policy = Policy::new(me(), None, &homes, &data, &config, None, None);
        let mut tree = scan(&t.0);
        let refused = |tree: &mut Tree, p: &Path| {
            let r = try_trash(tree, p, &policy);
            assert!(
                matches!(r, Err(Refusal::Protected(_))),
                "{}: {r:?}",
                p.display()
            );
        };
        for p in &whole {
            refused(&mut tree, &home.join(p));
            refused(&mut tree, &home.join(p).join("inner"));
        }
        for p in &only {
            refused(&mut tree, &home.join(p));
            try_trash(&mut tree, &home.join(p).join("inner"), &policy).unwrap();
        }
        for p in &free {
            try_trash(&mut tree, &home.join(p), &policy).unwrap();
        }
    }

    #[test]
    fn rule4_a_folder_holding_a_link_to_a_protected_file_is_refused() {
        let f = fix("r4-link");
        fs::hard_link(f.home.join(".bashrc"), f.home.join("work/sub/rc")).unwrap();
        let mut tree = scan(&f.t.0);
        let r = try_trash(&mut tree, &f.home.join("work"), &f.policy);
        assert!(
            matches!(&r, Err(Refusal::Protected(p)) if p.ends_with("work/sub/rc")),
            "{r:?}"
        );
        // The link itself, under a name no list has: refused by its inode.
        let r = try_trash(&mut tree, &f.home.join("work/sub/rc"), &f.policy);
        assert!(
            matches!(&r, Err(Refusal::Protected(p)) if p.ends_with("work/sub/rc")),
            "{r:?}"
        );
    }

    #[test]
    fn rule4_a_home_elsewhere_still_protects_the_passwd_home() {
        let f = fix("r4-elsewhere");
        let elsewhere = f.home.with_file_name("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let (home, data) = (
            elsewhere.into_os_string(),
            f.home.join(".local/share").into_os_string(),
        );
        let env = |k: &str| match k {
            "HOME" => Some(home.clone()),
            "XDG_DATA_HOME" => Some(data.clone()),
            _ => None,
        };
        let me = me();
        let with_passwd = Policy::from_env(false, me, me, env, Some(f.home.clone()), Some(NO_CAPS));
        assert_eq!(with_passwd.read_only(), None);
        let mut tree = scan(&f.t.0);
        let ssh = f.home.join(".ssh");
        let r = try_trash(&mut tree, &ssh, &with_passwd);
        assert!(matches!(r, Err(Refusal::Protected(_))), "{r:?}");
        // Without the passwd home this is the hole the passwd home closes.
        let no_passwd = Policy::from_env(false, me, me, env, None, Some(NO_CAPS));
        try_trash(&mut tree, &ssh, &no_passwd).unwrap();
    }

    #[test]
    fn rule11_home_set_to_a_folder_not_ones_own_is_read_only() {
        // HOME=/ with no passwd entry: nothing in any list would point at the real home.
        let env = |k: &str| (k == "HOME").then(|| OsString::from("/"));
        let p = Policy::from_env(false, 1000, 1000, env, None, Some(NO_CAPS));
        assert!(p
            .read_only()
            .is_some_and(|w| w.contains("HOME is not your")));
    }

    #[test]
    fn rule4_a_symlinked_home_protects_the_link_and_the_real_home() {
        let f = fix("r4-homelink");
        let link = f.home.with_file_name("home-link");
        symlink(&f.home, &link).unwrap();
        let raw = link.clone().into_os_string();
        let env = |k: &str| (k == "HOME").then(|| raw.clone());
        let p = Policy::from_env(false, me(), me(), env, None, Some(NO_CAPS));
        let mut tree = scan(&f.t.0);
        for item in [&link, &f.home.join(".ssh")] {
            let r = try_trash(&mut tree, item, &p);
            assert!(
                matches!(r, Err(Refusal::Protected(_))),
                "{}: {r:?}",
                item.display()
            );
        }
    }

    #[test]
    fn rule4_the_protect_file_only_adds() {
        let f = fix("r4-file");
        let has = |p: &Path, tree: bool| f.policy.protected.contains(&(p.to_path_buf(), tree));
        assert!(has(&f.home.join("keep"), true));
        assert!(
            has(&f.home.join(".ssh"), true),
            "a `!` line removes nothing"
        );
        assert!(has(&f.home.join("Downloads"), false));
        assert!(!f
            .policy
            .protected
            .iter()
            .any(|(p, _)| p.ends_with("skipped")));
    }

    #[test]
    fn rule4_marks_protected_paths_for_the_view() {
        let f = fix("r4-mark");
        let mut tree = scan(&f.t.0);
        f.policy.mark(&mut tree);
        let flagged =
            |p: &str| tree.node(tree.find(&f.home.join(p)).unwrap()).flags & PROTECTED != 0;
        assert!(flagged(".ssh/id") && flagged(".cache") && flagged("keep/f"));
        assert!(!flagged(".cache/app") && !flagged("work"));
    }

    #[test]
    fn rule5_a_folder_with_an_unreadable_folder_in_it_is_refused() {
        if me() == 0 {
            return;
        }
        let f = fix("r5-unreadable");
        let mut tree = scan(&f.t.0);
        let sub = f.home.join("work/sub");
        fs::set_permissions(&sub, Permissions::from_mode(0o000)).unwrap(); // test-only, after the scan
        let r = try_trash(&mut tree, &f.home.join("work"), &f.policy);
        fs::set_permissions(&sub, Permissions::from_mode(0o755)).unwrap(); // test-only
        assert!(
            matches!(&r, Err(Refusal::Unreadable(p)) if p.ends_with("work/sub")),
            "{r:?}"
        );
    }

    #[test]
    fn rule6_a_symlink_is_moved_not_what_it_points_to() {
        let f = fix("r6-link");
        let link = f.home.join(".cache/app/to-outside");
        symlink(f.t.0.join("outside"), &link).unwrap();
        let mut tree = scan(&f.t.0);
        assert_eq!(tree.node(tree.find(&link).unwrap()).kind(), SYMLINK);
        let done = try_trash(&mut tree, &link, &f.policy).unwrap();
        assert!(fs::symlink_metadata(&done.to)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(f.t.0.join("outside/inner/f")).unwrap(), b"outside");
    }

    #[test]
    fn rule6_an_item_replaced_since_the_scan_is_refused() {
        let f = fix("r6-replaced");
        let mut tree = scan(&f.t.0);
        let blob = f.home.join(".cache/app/blob");
        fs::write(f.home.join(".cache/app/new"), b"new").unwrap();
        fs::rename(f.home.join(".cache/app/new"), &blob).unwrap();
        let r = try_trash(&mut tree, &blob, &f.policy);
        assert!(matches!(r, Err(Refusal::Changed(_))), "{r:?}");
        assert_eq!(fs::read(&blob).unwrap(), b"new");
    }

    #[test]
    fn rule6_a_parent_swapped_for_a_symlink_is_refused() {
        let f = fix("r6-parent");
        let mut tree = scan(&f.t.0);
        fs::rename(f.home.join("work"), f.home.join("work2")).unwrap();
        symlink("work2", f.home.join("work")).unwrap();
        let r = try_trash(&mut tree, &f.home.join("work/sub"), &f.policy);
        assert!(matches!(r, Err(Refusal::Changed(_))), "{r:?}");
        assert!(f.home.join("work2/sub/x").exists());
    }

    #[test]
    fn rule6_a_folder_above_the_parent_swapped_for_a_symlink_is_refused() {
        // Not the item's parent (`sub`) but one above it (`work`): only a walk from `/` that
        // follows no link at any step sees it.
        let f = fix("r6-above");
        let mut tree = scan(&f.t.0);
        fs::rename(f.home.join("work"), f.home.join("work2")).unwrap();
        symlink("work2", f.home.join("work")).unwrap();
        let r = try_trash(&mut tree, &f.home.join("work/sub/x"), &f.policy);
        assert!(matches!(r, Err(Refusal::Changed(_))), "{r:?}");
        assert!(f.home.join("work2/sub/x").exists());
    }

    #[test]
    fn rule6_a_folder_above_with_search_permission_only_is_passed() {
        if me() == 0 {
            return;
        }
        let f = fix("r6-xonly");
        let mut tree = scan(&f.t.0);
        let work = f.home.join("work");
        fs::set_permissions(&work, Permissions::from_mode(0o100)).unwrap(); // test-only
        let r = try_trash(&mut tree, &f.home.join("work/sub/x"), &f.policy);
        fs::set_permissions(&work, Permissions::from_mode(0o755)).unwrap(); // test-only
        r.unwrap();
        assert!(!f.home.join("work/sub/x").exists());
    }

    #[test]
    fn rule9_the_scan_root_and_trashed_items_are_refused() {
        let f = fix("r9");
        let mut tree = scan(&f.home.join(".cache"));
        assert!(matches!(trash(&tree, 0, &f.policy), Err(Refusal::ScanRoot)));
        let app = f.home.join(".cache/app");
        try_trash(&mut tree, &app, &f.policy).unwrap();
        let r = try_trash(&mut tree, &app, &f.policy);
        assert!(matches!(r, Err(Refusal::AlreadyTrashed)), "{r:?}");
    }

    #[test]
    fn rule10_the_trash_must_be_a_real_folder_of_the_users() {
        let f = fix("r10");
        symlink(f.t.0.join("outside"), f.home.join(".local/share/Trash")).unwrap();
        let mut tree = scan(&f.t.0);
        let r = try_trash(&mut tree, &f.home.join(".cache/app/blob"), &f.policy);
        assert!(matches!(r, Err(Refusal::NoTrash(_))), "{r:?}");
        assert!(f.home.join(".cache/app/blob").exists());
        assert!(
            !f.t.0.join("outside/files").exists(),
            "nothing made through the link"
        );
    }

    #[test]
    fn own_file_in_a_sticky_world_writable_folder_can_be_trashed() {
        // The /tmp case: the guard asks nothing of the parent folder's owner or mode.
        let f = fix("sticky");
        let tmp = f.home.join("tmpish");
        fs::create_dir(&tmp).unwrap();
        fs::write(tmp.join("mine"), b"mine").unwrap();
        fs::set_permissions(&tmp, Permissions::from_mode(0o1777)).unwrap(); // test-only
        let mut tree = scan(&f.t.0);
        try_trash(&mut tree, &tmp.join("mine"), &f.policy).unwrap();
        assert!(!tmp.join("mine").exists());
    }

    #[test]
    fn rule1_and_5_real_mounts_in_a_sandbox() {
        let t = TempDir::new("bwrap-guard");
        let home = t.0.join("home");
        for d in [
            "home/.local/share",
            "home2/.local/share/Trash/files",
            "run",
            "root/sub/tm",
            "root/bind",
            "root/plain",
            "root/plain2",
            "src",
        ] {
            fs::create_dir_all(t.0.join(d)).unwrap();
        }
        for f in ["src/f", "root/plain/f", "root/plain2/f", "root/sub/f"] {
            fs::write(t.0.join(f), b"f").unwrap();
        }
        let (tm, bind, src) = (
            t.0.join("root/sub/tm"),
            t.0.join("root/bind"),
            t.0.join("src"),
        );
        let not_ours = t.0.join("home2/.local/share/Trash/files");
        // A user and mount namespace: the mounts exist only inside it. In it, root's /etc
        // belongs to nobody.
        let sandbox = |cmd: &[&OsStr]| {
            Command::new("bwrap")
                .args(["--dev-bind", "/", "/", "--tmpfs"])
                .arg(&tm)
                .arg("--bind")
                .args([&src, &bind])
                .args(["--ro-bind", "/etc"])
                .arg(&not_ours)
                .arg("--")
                .args(cmd)
                .env("TERMDISK_SANDBOX", &t.0)
                .status()
        };
        if !sandbox(&[OsStr::new("true")]).is_ok_and(|s| s.success()) {
            eprintln!("skipped: bwrap is missing or cannot make a sandbox here");
            return;
        }
        let exe = env::current_exe().unwrap();
        let test = [
            "--exact",
            "guard::tests::in_the_sandbox",
            "--ignored",
            "--quiet",
        ];
        let mut cmd = vec![exe.as_os_str()];
        cmd.extend(test.iter().map(OsStr::new));
        let status = sandbox(&cmd).unwrap();
        assert!(status.success(), "the checks inside the sandbox failed");
        assert!(home.exists() && src.join("f").exists());
    }

    /// Run by `rule1_and_5_real_mounts_in_a_sandbox` inside bwrap: `root/sub/tm` is a tmpfs,
    /// `root/bind` a bind mount of `src` (same filesystem), `home2`'s `Trash/files` is /etc.
    #[test]
    #[ignore = "run inside bwrap by rule1_and_5_real_mounts_in_a_sandbox"]
    fn in_the_sandbox() {
        let Some(t) = env::var_os("TERMDISK_SANDBOX").map(PathBuf::from) else {
            return;
        };
        let t = fs::canonicalize(t).unwrap();
        let home = t.join("home");
        let policy = policy_for(&home, &t, None);
        let root = t.join("root");
        let mut tree = scan(&root);
        for p in ["sub", "sub/tm", "bind"] {
            let r = try_trash(&mut tree, &root.join(p), &policy);
            assert!(matches!(r, Err(Refusal::Mount(_))), "{p}: {r:?}");
        }
        // On the tmpfs: another filesystem than the trash.
        fs::write(root.join("sub/tm/f"), b"f").unwrap();
        let mut on_tmpfs = scan(&root.join("sub/tm"));
        let r = try_trash(&mut on_tmpfs, &root.join("sub/tm/f"), &policy);
        assert!(matches!(r, Err(Refusal::OtherFilesystem(_))), "{r:?}");
        // Inside the bind mount: same device as the trash, but a rename across mounts fails.
        let mut in_bind = scan(&root.join("bind"));
        let r = try_trash(&mut in_bind, &root.join("bind/f"), &policy);
        let exdev = matches!(&r, Err(Refusal::Failed(e)) if e.raw_os_error() == Some(libc::EXDEV));
        assert!(exdev, "{r:?}");
        assert!(root.join("bind/f").exists());
        assert_eq!(
            fs::read_dir(home.join(".local/share/Trash/info"))
                .unwrap()
                .count(),
            0
        );
        try_trash(&mut tree, &root.join("plain"), &policy).unwrap();
        // A trash whose `files` folder is not the user's (root's /etc, seen as nobody here).
        let other_home = policy_for(&t.join("home2"), &t, None);
        let r = try_trash(&mut tree, &root.join("plain2"), &other_home);
        assert!(
            matches!(&r, Err(Refusal::NoTrash(e)) if e.contains("not yours")),
            "{r:?}"
        );
        assert!(root.join("plain2/f").exists());
    }
}
