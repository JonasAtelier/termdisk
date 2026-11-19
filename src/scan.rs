// SPDX-License-Identifier: GPL-3.0-only

//! The scan: a few threads walk the folders, one thread builds the tree.
//!
//! Walkers take folders from a shared stack, read each one and send its entries to the
//! builder as one batch. A folder is opened without following a symlink and must still be the
//! inode its parent's listing saw; every entry is lstat'ed relative to it with `statx` (no
//! automount; mount roots, bind mounts included, are seen and not entered). A subfolder gets
//! its id before the batch goes out and is put on the stack only after, so its own batch always
//! reaches the builder after the batch that names it (the channel keeps order); the builder
//! needs no lock and never waits for a parent.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::sys::{self, Dir, Stat};
use crate::tree::{NodeId, Tree, DIR, HARDLINK_DUP, KIND, LINKED, MOUNT, NOT_OWNED, UNREADABLE};

/// A folder to read: its id, its path, and the inode the scan saw there.
type Job = (u32, PathBuf, u64);

// ponytail: a fixed cap on walker threads; retune it with scripts/bench.sh rather than add a flag.
const MAX_THREADS: usize = 8;
/// `Entry::dir_id` of an entry that will not be walked.
const NO_DIR: u32 = u32::MAX;

/// What the scan has found so far, for a progress display.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    /// Disk usage found so far, in bytes.
    pub bytes: AtomicU64,
    /// Folders or entries that could not be read.
    pub errors: AtomicU64,
    /// A folder being read now.
    pub current: Mutex<PathBuf>,
    cancel: AtomicBool,
}

/// A running scan. Dropping it cancels the walk.
pub struct Scan {
    shared: Arc<Shared>,
    /// The builder; taken by `join`.
    handle: Option<JoinHandle<Option<Tree>>>,
}

impl Drop for Scan {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.cancel();
        }
    }
}

impl Scan {
    pub fn progress(&self) -> &Progress {
        &self.shared.progress
    }

    /// Stops the walk; `join` then returns `None`.
    pub fn cancel(&self) {
        self.shared.progress.cancel.store(true, Relaxed);
        let _q = self.shared.queue.lock().unwrap();
        self.shared.wake.notify_all();
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Waits for the tree; `None` if the scan was cancelled. A panic in the builder goes on
    /// here.
    pub fn join(mut self) -> Option<Tree> {
        let handle = self
            .handle
            .take()
            .expect("joined once: join takes the scan");
        let tree = handle
            .join()
            .unwrap_or_else(|p| std::panic::resume_unwind(p));
        tree.filter(|_| !self.shared.progress.cancel.load(Relaxed))
    }
}

/// Starts scanning `root` (made absolute; a symlink given as `root` itself is followed, as
/// `cd` would) as user `euid`, for the `NOT_OWNED` flag. The walk stays on `root`'s device and
/// does not enter mount points.
pub fn scan(root: &Path, euid: u32) -> io::Result<Scan> {
    let root = fs::canonicalize(root)?;
    let st = sys::stat_path(&root)?;
    let mut tree = Tree::new(root.clone(), st.dev);
    tree.push(
        0,
        b"",
        st.blocks * 512,
        st.size,
        st.ino,
        st.mtime,
        flags(&st, euid),
    );
    let jobs = if st.kind == DIR {
        vec![(0, root, st.ino)]
    } else {
        Vec::new()
    };
    let shared = Arc::new(Shared::new(st.dev, euid, jobs));
    let threads = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(MAX_THREADS);
    let (tx, rx) = sync_channel(4096);
    for _ in 0..threads {
        let (sh, tx) = (shared.clone(), tx.clone());
        thread::Builder::new()
            .name("termdisk-walk".into())
            .spawn(move || walk(&sh, tx))?;
    }
    drop(tx);
    let sh = shared.clone();
    let handle = thread::Builder::new()
        .name("termdisk-scan".into())
        .spawn(move || build(&sh, rx, tree))?;
    Ok(Scan {
        shared,
        handle: Some(handle),
    })
}

/// A node's kind, and `NOT_OWNED` unless `euid` owns it.
fn flags(st: &Stat, euid: u32) -> u16 {
    st.kind | if st.uid == euid { 0 } else { NOT_OWNED }
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// The next folder id; folder 0 is the root.
    next_dir: AtomicU32,
    progress: Progress,
    dev: u64,
    euid: u32,
}

struct Queue {
    /// Folders to read.
    jobs: Vec<Job>,
    /// Walkers reading a folder now (they may add more jobs).
    busy: usize,
}

/// One folder's entries; their names are concatenated in `names`.
struct Batch {
    dir_id: u32,
    names: Vec<u8>,
    entries: Vec<Entry>,
    errors: u64,
}

struct Entry {
    alloc: u64,
    apparent: u64,
    ino: u64,
    mtime: i64,
    /// The id it is walked under, or `NO_DIR`.
    dir_id: u32,
    name_len: u8,
    flags: u16,
    /// A non-folder with more than one hard link.
    linked: bool,
}

impl Shared {
    fn new(dev: u64, euid: u32, jobs: Vec<Job>) -> Shared {
        Shared {
            queue: Mutex::new(Queue { jobs, busy: 0 }),
            wake: Condvar::new(),
            next_dir: AtomicU32::new(1),
            progress: Progress::default(),
            dev,
            euid,
        }
    }

    /// The next folder to read; `None` once there is none and no walker can add one.
    fn pop(&self) -> Option<Job> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if self.progress.cancel.load(Relaxed) {
                return None;
            }
            if let Some(job) = q.jobs.pop() {
                q.busy += 1;
                return Some(job);
            }
            if q.busy == 0 {
                return None;
            }
            q = self.wake.wait(q).unwrap();
        }
    }

    /// A walker finished a folder and found `jobs` under it.
    fn done(&self, jobs: Vec<Job>) {
        let mut q = self.queue.lock().unwrap();
        q.busy -= 1;
        if q.busy == 0 && q.jobs.is_empty() && jobs.is_empty() {
            self.wake.notify_all(); // the walk is over: let every waiting walker see it
        }
        for _ in 0..jobs.len() {
            self.wake.notify_one();
        }
        q.jobs.extend(jobs);
    }
}

/// Held by a walker while it reads a folder. If the walker panics, this gives its place back
/// and stops the scan, so the other walkers and the builder end instead of waiting for it.
struct Slot<'a>(&'a Shared);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.progress.cancel.store(true, Relaxed);
            self.0.done(Vec::new());
        }
    }
}

fn walk(sh: &Shared, tx: SyncSender<Batch>) {
    let mut buf = Vec::new();
    while let Some((dir_id, path, ino)) = sh.pop() {
        let _slot = Slot(sh);
        if let Ok(mut current) = sh.progress.current.try_lock() {
            current.clone_from(&path);
        }
        let mut b = Batch {
            dir_id,
            names: Vec::new(),
            entries: Vec::new(),
            errors: 0,
        };
        let mut subdirs = Vec::new();
        if read_folder(sh, &path, ino, &mut buf, &mut b, &mut subdirs).is_err() {
            b.errors += 1;
        }
        // Send first, then queue the subfolders: their batches must come after this one.
        if tx.send(b).is_err() {
            sh.progress.cancel.store(true, Relaxed);
        }
        sh.done(subdirs);
    }
}

/// Reads one folder into `b`, its subfolders to walk into `subdirs`. An error means the folder
/// could not be read through, or is no longer the inode `ino` its parent's listing saw.
fn read_folder(
    sh: &Shared,
    path: &Path,
    ino: u64,
    buf: &mut Vec<u8>,
    b: &mut Batch,
    subdirs: &mut Vec<Job>,
) -> io::Result<()> {
    let dir = Dir::open(path)?;
    let own = dir.stat()?;
    if (own.dev, own.ino) != (sh.dev, ino) {
        return Err(io::Error::other("changed during the scan"));
    }
    dir.each(buf, |name| {
        let name_bytes = name.to_bytes();
        if name_bytes.len() > 255 {
            b.errors += 1; // over Linux's NAME_MAX; keeps the length in a u8
            return;
        }
        let Ok(st) = dir.stat_at(name) else {
            b.errors += 1;
            return;
        };
        let mut e = Entry {
            alloc: st.blocks * 512,
            apparent: st.size,
            ino: st.ino,
            mtime: st.mtime,
            dir_id: NO_DIR,
            name_len: name_bytes.len() as u8,
            flags: flags(&st, sh.euid),
            linked: false,
        };
        if st.kind != DIR {
            e.linked = st.nlink > 1;
        } else if st.dev != sh.dev || st.mount_root {
            e.flags |= MOUNT; // listed with its own size, not entered
        } else {
            e.dir_id = sh.next_dir.fetch_add(1, Relaxed);
            let sub = path.join(OsStr::from_bytes(name_bytes));
            subdirs.push((e.dir_id, sub, st.ino));
        }
        b.names.extend_from_slice(name_bytes);
        b.entries.push(e);
    })
}

fn build(sh: &Shared, rx: Receiver<Batch>, mut tree: Tree) -> Option<Tree> {
    let p = &sh.progress;
    // Folder id -> node. Ids not reached yet hold NodeId::MAX, so a batch that came before the
    // batch naming its folder would panic rather than land on the wrong node.
    let mut node_of: Vec<NodeId> = vec![0];
    let mut seen = HashSet::new(); // inodes of files with several links, counted once
    for b in rx {
        let parent = node_of[b.dir_id as usize];
        let first = NodeId::try_from(tree.len()).expect("tree too large: 4 G entries");
        tree.add_children(parent, first, b.entries.len() as u32);
        if b.errors > 0 {
            tree.set_flags(parent, UNREADABLE);
            tree.errors += b.errors;
            p.errors.fetch_add(b.errors, Relaxed);
        }
        let (mut off, mut files, mut dirs, mut bytes) = (0, 0, 0, 0);
        for e in &b.entries {
            let mut flags = e.flags;
            if e.linked && !seen.insert(e.ino) {
                flags |= HARDLINK_DUP;
            } else {
                flags |= if e.linked { LINKED } else { 0 };
                bytes += e.alloc;
            }
            let name = &b.names[off..off + e.name_len as usize];
            off += e.name_len as usize;
            let id = tree.push(parent, name, e.alloc, e.apparent, e.ino, e.mtime, flags);
            if e.dir_id != NO_DIR {
                let i = e.dir_id as usize;
                if node_of.len() <= i {
                    node_of.resize(i + 1, NodeId::MAX);
                }
                node_of[i] = id;
            }
            if flags & KIND == DIR {
                dirs += 1;
            } else {
                files += 1;
            }
        }
        p.files.fetch_add(files, Relaxed);
        p.dirs.fetch_add(dirs, Relaxed);
        p.bytes.fetch_add(bytes, Relaxed);
    }
    if p.cancel.load(Relaxed) {
        return None;
    }
    tree.finish();
    Some(tree)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::SYMLINK;
    use crate::TempDir;
    use std::fs::Permissions;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::time::Duration;

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    /// Scans `root` to the end: the tree and the error count.
    fn run(root: &Path, euid: u32) -> (Tree, u64) {
        let s = scan(root, euid).unwrap();
        while !s.is_finished() {
            thread::sleep(Duration::from_millis(1));
        }
        let errors = s.progress().errors.load(Relaxed);
        (s.join().unwrap(), errors)
    }

    /// Disk usage, apparent size and item count under `dir`, the slow obvious way.
    fn expected(dir: &Path, seen: &mut HashSet<u64>) -> (u64, u64, u32) {
        let md = fs::symlink_metadata(dir).unwrap();
        let (mut alloc, mut apparent, mut items) = (md.blocks() * 512, md.size(), 0);
        for ent in fs::read_dir(dir).unwrap() {
            let p = ent.unwrap().path();
            let md = fs::symlink_metadata(&p).unwrap();
            items += 1;
            if md.is_dir() {
                let (a, b, n) = expected(&p, seen);
                (alloc, apparent, items) = (alloc + a, apparent + b, items + n);
            } else if md.nlink() == 1 || seen.insert(md.ino()) {
                (alloc, apparent) = (alloc + md.blocks() * 512, apparent + md.size());
            }
        }
        (alloc, apparent, items)
    }

    fn fixture(r: &Path) {
        fs::create_dir_all(r.join("a/b/c")).unwrap();
        fs::create_dir(r.join("empty-dir")).unwrap();
        fs::write(r.join("a/one"), vec![1u8; 10_000]).unwrap();
        fs::write(r.join("a/b/two"), vec![2u8; 100_000]).unwrap();
        fs::write(r.join("a/b/c/three"), b"x").unwrap();
        fs::write(r.join("empty"), b"").unwrap();
        fs::hard_link(r.join("a/b/two"), r.join("two-again")).unwrap();
        symlink("a/b", r.join("link")).unwrap();
        fs::write(r.join(OsStr::from_bytes(b"caf\xe9\n -x")), b"abc").unwrap(); // not UTF-8
    }

    fn count(tree: &Tree, flag: u16) -> usize {
        (0..tree.len() as NodeId)
            .filter(|&i| tree.node(i).flags & flag != 0)
            .count()
    }

    /// Every node's path leads to its inode on disk and back to it, and every node but the
    /// root is exactly one folder's child.
    fn check_links_paths_and_inodes(tree: &Tree) {
        let mut children = 0;
        for id in 0..tree.len() as NodeId {
            for k in tree.children(id) {
                assert_eq!(tree.parent(k), Some(id));
                children += 1;
            }
            let path = tree.path(id);
            assert_eq!(tree.find(&path), Some(id));
            let ino = fs::symlink_metadata(&path).unwrap().ino();
            assert_eq!(ino, tree.node(id).ino, "{}", path.display());
        }
        assert_eq!(children, tree.len() - 1);
    }

    #[test]
    fn totals_match_an_independent_walk() {
        let t = TempDir::new("totals");
        fixture(&t.0);
        let (tree, errors) = run(&t.0, me());
        let root = tree.node(tree.root());
        assert_eq!(
            (root.alloc, root.apparent, root.items),
            expected(&t.0, &mut HashSet::new())
        );
        assert_eq!((errors, tree.len()), (0, 12));
        assert_eq!(
            count(&tree, HARDLINK_DUP),
            1,
            "a hard-linked file counts once"
        );
        assert_eq!(count(&tree, LINKED), 1, "the counted link is marked");
    }

    #[test]
    fn tree_links_paths_and_inodes_agree_with_the_disk() {
        let t = TempDir::new("paths");
        fixture(&t.0);
        let (tree, _) = run(&t.0, me());
        check_links_paths_and_inodes(&tree);
        let odd = tree
            .find(Path::new(OsStr::from_bytes(b"caf\xe9\n -x")))
            .unwrap();
        assert_eq!(tree.node(odd).apparent, 3);
        assert_eq!(
            tree.find(Path::new("a/b/c/three"))
                .map(|id| tree.node(id).apparent),
            Some(1)
        );
        assert_eq!(tree.find(Path::new("a/nothing")), None);
    }

    #[test]
    fn symlinks_are_listed_not_followed() {
        let t = TempDir::new("links");
        let (root, outside) = (t.0.join("root"), t.0.join("outside"));
        fs::create_dir_all(outside.join("inner")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(outside.join("big"), vec![0u8; 1 << 20]).unwrap();
        symlink(&outside, root.join("dirlink")).unwrap();
        symlink(outside.join("big"), root.join("filelink")).unwrap();
        let (tree, _) = run(&root, me());
        for name in ["dirlink", "filelink"] {
            let id = tree.find(Path::new(name)).unwrap();
            assert_eq!(tree.node(id).kind(), SYMLINK);
            assert!(tree.children(id).is_empty());
        }
        let r = tree.node(tree.root());
        assert_eq!(r.items, 2);
        assert!(
            r.apparent < 1 << 20 && r.alloc < 1 << 20,
            "the 1 MiB file behind a link is not counted"
        );
    }

    #[test]
    fn unreadable_folder_is_flagged_up_to_the_root() {
        if me() == 0 {
            return; // root reads anything
        }
        let t = TempDir::new("unreadable");
        let locked = t.0.join("open/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::create_dir(t.0.join("fine")).unwrap();
        fs::write(locked.join("f"), b"x").unwrap();
        fs::set_permissions(&locked, Permissions::from_mode(0o000)).unwrap(); // test-only
        let (tree, errors) = run(&t.0, me());
        fs::set_permissions(&locked, Permissions::from_mode(0o755)).unwrap(); // test-only
        let flagged = |p: &str| tree.node(tree.find(Path::new(p)).unwrap()).flags & UNREADABLE != 0;
        assert!(flagged("open/locked") && flagged("open") && flagged(""));
        assert!(!flagged("fine"));
        assert_eq!((errors, tree.errors), (1, 1));
    }

    #[test]
    fn a_folder_swapped_after_its_listing_is_not_read() {
        // The walk opens a folder only if it is still the inode its parent's listing saw.
        let t = TempDir::new("swapped");
        fs::create_dir(t.0.join("d")).unwrap();
        fs::write(t.0.join("d/f"), b"x").unwrap();
        let ino = fs::symlink_metadata(t.0.join("d")).unwrap().ino();
        let sh = Shared::new(fs::symlink_metadata(&t.0).unwrap().dev(), me(), Vec::new());
        let read = |ino| {
            let mut b = Batch {
                dir_id: 0,
                names: Vec::new(),
                entries: Vec::new(),
                errors: 0,
            };
            let r = read_folder(
                &sh,
                &t.0.join("d"),
                ino,
                &mut Vec::new(),
                &mut b,
                &mut Vec::new(),
            );
            (r.is_ok(), b.entries.len())
        };
        assert_eq!(read(ino), (true, 1));
        assert_eq!(read(ino + 1), (false, 0));
        fs::rename(t.0.join("d"), t.0.join("e")).unwrap();
        symlink("e", t.0.join("d")).unwrap();
        assert_eq!(
            read(ino),
            (false, 0),
            "a symlink put in its place is not followed"
        );
    }

    #[test]
    fn a_relative_root_is_made_absolute() {
        let t = TempDir::new("relative");
        fixture(&t.0);
        // The temp folder as seen from the current folder: ../../tmp/termdisk-...
        let cwd = std::env::current_dir().unwrap();
        let mut rel = PathBuf::new();
        for _ in 1..cwd.components().count() {
            rel.push("..");
        }
        rel.push(t.0.strip_prefix("/").unwrap());
        assert!(rel.is_relative());
        let (tree, _) = run(&rel, me());
        assert_eq!(tree.root_path(), fs::canonicalize(&t.0).unwrap());
        check_links_paths_and_inodes(&tree);
        let (abs, _) = run(&t.0, me());
        assert_eq!(tree.node(0).alloc, abs.node(0).alloc);
    }

    #[test]
    fn thousands_of_folders_keep_their_names_and_inodes() {
        let t = TempDir::new("many");
        for a in 0..10 {
            for b in 0..10 {
                for c in 0..30 {
                    fs::create_dir_all(t.0.join(format!("a{a}/b{b}/c{c}"))).unwrap();
                }
                fs::write(t.0.join(format!("a{a}/b{b}/f")), vec![b; 100 * a]).unwrap();
            }
        }
        let (tree, errors) = run(&t.0, me());
        assert_eq!((errors, tree.len()), (0, 1 + 10 + 100 * 2 + 3000));
        check_links_paths_and_inodes(&tree);
        let root = tree.node(0);
        assert_eq!(
            (root.alloc, root.apparent, root.items),
            expected(&t.0, &mut HashSet::new())
        );
    }

    #[test]
    fn owner_flag_follows_the_scanning_user() {
        let t = TempDir::new("owner");
        fixture(&t.0);
        let all = |tree: &Tree, set: bool| {
            (0..tree.len() as NodeId).all(|i| (tree.node(i).flags & NOT_OWNED != 0) == set)
        };
        assert!(all(&run(&t.0, me()).0, false));
        assert!(all(&run(&t.0, me().wrapping_add(1)).0, true));
    }

    #[test]
    fn cancelled_scan_gives_no_tree() {
        let t = TempDir::new("cancel");
        fixture(&t.0);
        let s = scan(&t.0, me()).unwrap();
        s.cancel();
        assert!(s.join().is_none());
    }

    #[test]
    fn walkers_take_no_folder_once_cancelled() {
        let sh = Shared::new(
            0,
            me(),
            vec![(0, PathBuf::from("/a"), 0), (1, PathBuf::from("/b"), 0)],
        );
        assert!(sh.pop().is_some());
        sh.progress.cancel.store(true, Relaxed);
        assert!(
            sh.pop().is_none(),
            "a folder is still queued, but the scan is cancelled"
        );
    }

    #[test]
    fn dropping_a_scan_cancels_it() {
        let t = TempDir::new("drop");
        fixture(&t.0);
        let s = scan(&t.0, me()).unwrap();
        let sh = s.shared.clone();
        drop(s);
        assert!(sh.progress.cancel.load(Relaxed));
        // Every walker and the builder let go of `sh` once they end.
        for _ in 0..5000 {
            if Arc::strong_count(&sh) == 1 {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("the scan's threads did not end");
    }

    #[test]
    fn a_walker_panic_gives_its_place_back_and_stops_the_scan() {
        let sh = Arc::new(Shared::new(0, me(), vec![(0, PathBuf::from("/a"), 0)]));
        let s2 = sh.clone();
        let r = thread::spawn(move || {
            let _job = s2.pop();
            let _slot = Slot(&s2);
            panic!("test: a walker panics while reading");
        })
        .join();
        assert!(r.is_err());
        assert!(sh.progress.cancel.load(Relaxed));
        assert_eq!(sh.queue.lock().unwrap().busy, 0);
    }

    #[test]
    fn a_file_as_root_is_a_one_node_tree() {
        let t = TempDir::new("file");
        fs::write(t.0.join("f"), vec![0u8; 3000]).unwrap();
        let (tree, errors) = run(&t.0.join("f"), me());
        assert_eq!((tree.len(), errors, tree.node(0).apparent), (1, 0, 3000));
    }
}
