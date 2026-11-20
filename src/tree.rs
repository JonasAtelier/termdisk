// SPDX-License-Identifier: GPL-3.0-only

//! The scan result: every file and folder under the scan root, in one arena.
//!
//! A folder's children sit next to each other in `nodes` (the scanner adds a whole folder at
//! once), so a node needs no sibling links; a child always comes after its parent, which lets
//! `finish` add sizes up in one backward pass. Names live in one shared byte buffer, in node
//! order, so a name ends where the next one starts.
//!
//! A `Node` is 40 bytes: what only folders have (where their children are, how many items lie
//! under them) is kept apart, one `Folder` per folder, so files do not carry it. On a home of
//! 1.24 million entries the tree went from 72.6 to 58.3 bytes per entry (names included).

use std::ffi::OsStr;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// A node's index in the tree; the root is 0.
pub type NodeId = u32;

/// The kind, in the two low bits of `Node::flags`.
pub const FILE: u16 = 0;
pub const DIR: u16 = 1;
pub const SYMLINK: u16 = 2;
pub const OTHER: u16 = 3;
pub const KIND: u16 = 0b11;
/// It is not owned by the user the scan ran as.
pub const NOT_OWNED: u16 = 1 << 2;
/// It is a folder that could not be read completely (itself, or an entry in it).
pub const UNREADABLE: u16 = 1 << 3;
/// It is a mount point: the root of another filesystem or of a bind mount. The scan lists a
/// mount point with its own lstat size (as `find -xdev` counts it; uutils `du -x` 0.10 leaves
/// it out) but does not enter it, so it has no children.
pub const MOUNT: u16 = 1 << 4;
/// A file with other hard links, this one carrying the size: trashing it frees nothing while
/// another link remains.
pub const LINKED: u16 = 1 << 5;
/// A hard link to a file already counted elsewhere in the tree: shown, not added to totals.
pub const HARDLINK_DUP: u16 = 1 << 6;
/// It is, or lies inside, a protected path (`Policy::mark`).
pub const PROTECTED: u16 = 1 << 7;
/// Moved to the trash; no longer counted in its folders' totals.
pub const TRASHED: u16 = 1 << 8;
/// Something under it has one of the `LOCKED` flags: it holds an item that cannot be trashed,
/// so it cannot be trashed either.
pub const HOLDS: u16 = 1 << 9;
/// The flags that keep an item itself from the trash (the view shows them with a lock). Like
/// `HOLDS`, a hint for the view only: the trash guard checks everything again, on the disk,
/// when asked to act.
pub const LOCKED: u16 = PROTECTED | NOT_OWNED | MOUNT | UNREADABLE;

#[derive(Clone, Copy, Debug)]
pub struct Node {
    /// Disk usage in bytes (`st_blocks * 512`); a folder's includes everything under it.
    pub alloc: u64,
    /// Apparent size in bytes (`st_size`); a folder's includes everything under it.
    pub apparent: u64,
    /// The inode number, whole: the trash guard compares it with the disk before it moves
    /// anything.
    pub ino: u64,
    /// Modification time, seconds since the epoch: 0 for a time before 1970, `u32::MAX` for one
    /// after 2106 (the view shows dates and ages only).
    pub mtime: u32,
    name_off: u32,
    pub parent: NodeId,
    pub flags: u16,
}

/// What only a folder has.
#[derive(Clone, Copy, Debug, Default)]
struct Folder {
    first_child: NodeId,
    n_children: u32,
    /// How many files and folders are under it.
    items: u32,
}

impl Node {
    pub fn kind(&self) -> u16 {
        self.flags & KIND
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == DIR
    }
}

pub struct Tree {
    nodes: Vec<Node>,
    names: Vec<u8>,
    /// One per folder node, in node order: a folder's is found by counting the folders before
    /// it (`folder`), one bit per node in `is_folder` and a count for each 64 nodes in
    /// `folders_before`. That costs files 1.5 bits each instead of 12 bytes.
    folders: Vec<Folder>,
    is_folder: Vec<u64>,
    folders_before: Vec<u32>,
    root_path: PathBuf,
    /// The device of the scan root. Every node is on it, except that a mount point's `ino` is
    /// the mounted root's, which may be on another device.
    pub dev: u64,
    /// How many folders or entries the scan could not read.
    pub errors: u64,
}

impl Tree {
    /// An empty tree; the first node pushed is the root (`push(0, b"", ..)`).
    pub(crate) fn new(root_path: PathBuf, dev: u64) -> Tree {
        Tree {
            nodes: Vec::new(),
            names: Vec::new(),
            folders: Vec::new(),
            is_folder: Vec::new(),
            folders_before: Vec::new(),
            root_path,
            dev,
            errors: 0,
        }
    }

    /// How many folders come before node `id` (`id` may be one past the last node).
    fn folders_until(&self, id: usize) -> usize {
        let (word, bit) = (id / 64, id % 64);
        match self.is_folder.get(word) {
            Some(&bits) => {
                let below = bits & ((1u64 << bit) - 1);
                self.folders_before[word] as usize + below.count_ones() as usize
            }
            None => self.folders.len(),
        }
    }

    /// Where node `id`'s `Folder` is; `None` if it is not a folder.
    fn folder(&self, id: NodeId) -> Option<usize> {
        let id = id as usize;
        // Past the last node, the last word's bits are 0: it would pass for a file.
        debug_assert!(id < self.nodes.len(), "node {id} of {}", self.nodes.len());
        let is = self.is_folder[id / 64] >> (id % 64) & 1 != 0;
        is.then(|| self.folders_until(id))
    }

    /// Records that folder `parent`'s `n` children are the nodes from `first` on (pushed next).
    pub(crate) fn add_children(&mut self, parent: NodeId, first: NodeId, n: u32) {
        let f = self.folder(parent).expect("only a folder has children");
        self.folders[f].first_child = first;
        self.folders[f].n_children = n;
    }

    /// Appends one node; its parent's `add_children` call must cover it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push(
        &mut self,
        parent: NodeId,
        name: &[u8],
        alloc: u64,
        apparent: u64,
        ino: u64,
        mtime: i64,
        flags: u16,
    ) -> NodeId {
        // ponytail: u32 ids and name offsets cap a tree at 4 G nodes and 4 GiB of names (about
        // 240 M entries at 18 B a name, ~18 GB of RAM); past that the scan panics rather than
        // mix names up. Widen them if that ever matters.
        let id = NodeId::try_from(self.nodes.len()).expect("tree too large: 4 G entries");
        let name_off = u32::try_from(self.names.len()).expect("tree too large: 4 GiB of names");
        let (word, bit) = (id as usize / 64, id % 64);
        if bit == 0 {
            self.is_folder.push(0);
            self.folders_before.push(self.folders.len() as u32);
        }
        if flags & KIND == DIR {
            self.is_folder[word] |= 1 << bit;
            self.folders.push(Folder::default());
        }
        self.nodes.push(Node {
            alloc,
            apparent,
            ino,
            mtime: mtime.clamp(0, u32::MAX.into()) as u32,
            name_off,
            parent,
            flags,
        });
        self.names.extend_from_slice(name);
        id
    }

    pub(crate) fn set_flags(&mut self, id: NodeId, flags: u16) {
        self.nodes[id as usize].flags |= flags;
    }

    /// Marks `id` `PROTECTED` and, for a protected subtree, everything under it; its ancestors
    /// `HOLDS` (they contain it).
    pub(crate) fn mark_protected(&mut self, id: NodeId, subtree: bool) {
        let mut stack = vec![id];
        while let Some(i) = stack.pop() {
            self.nodes[i as usize].flags |= PROTECTED;
            if subtree {
                stack.extend(self.children(i));
            }
        }
        let mut up = id;
        while let Some(p) = self.parent(up) {
            self.nodes[p as usize].flags |= HOLDS;
            up = p;
        }
    }

    /// Marks `id` `TRASHED` and takes what it counted out of its ancestors' totals. The `HOLDS`
    /// they took from it stays (a hint that may now be too careful, never too lax).
    /// Does nothing if `id` or a folder above it is marked already.
    pub fn mark_trashed(&mut self, id: NodeId) {
        let mut up = Some(id);
        while let Some(i) = up {
            if self.nodes[i as usize].flags & TRASHED != 0 {
                return;
            }
            up = self.parent(i);
        }
        let n = self.nodes[id as usize];
        let items = self.items(id);
        self.nodes[id as usize].flags |= TRASHED;
        let (alloc, apparent) = if n.flags & HARDLINK_DUP == 0 {
            (n.alloc, n.apparent)
        } else {
            (0, 0)
        };
        let mut up = id;
        while let Some(p) = self.parent(up) {
            let p_node = &mut self.nodes[p as usize];
            p_node.alloc -= alloc;
            p_node.apparent -= apparent;
            if let Some(f) = self.folder(p) {
                self.folders[f].items -= 1 + items;
            }
            up = p;
        }
    }

    /// Adds every node's sizes into its parent's and marks the parent `HOLDS` if the node is
    /// locked or holds a locked item; counts each folder's items; then gives back the spare
    /// capacity.
    pub(crate) fn finish(&mut self) {
        for i in (1..self.nodes.len()).rev() {
            let n = self.nodes[i];
            let p = &mut self.nodes[n.parent as usize];
            if n.flags & HARDLINK_DUP == 0 {
                p.alloc += n.alloc;
                p.apparent += n.apparent;
            }
            if n.flags & (LOCKED | HOLDS) != 0 {
                p.flags |= HOLDS;
            }
        }
        // A folder holds its children and what its child folders hold. Those come after it in
        // `folders` (a child comes after its parent), so going backwards finds them counted, and
        // they lie together there as their nodes do: two counts per folder, not one per node.
        for f in (0..self.folders.len()).rev() {
            let Folder {
                first_child,
                n_children,
                ..
            } = self.folders[f];
            let first = self.folders_until(first_child as usize);
            let end = self.folders_until((first_child + n_children) as usize);
            let below: u32 = self.folders[first..end].iter().map(|c| c.items).sum();
            self.folders[f].items = n_children + below;
        }
        self.nodes.shrink_to_fit();
        self.names.shrink_to_fit();
        self.folders.shrink_to_fit();
        self.is_folder.shrink_to_fit();
        self.folders_before.shrink_to_fit();
    }

    pub fn root(&self) -> NodeId {
        0
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The memory the tree holds, in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.names.capacity()
            + self.folders.capacity() * std::mem::size_of::<Folder>()
            + self.is_folder.capacity() * 8
            + self.folders_before.capacity() * 4
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// The node's name; the root's is its whole path.
    pub fn name(&self, id: NodeId) -> &OsStr {
        if id == 0 {
            return self.root_path.as_os_str();
        }
        // Names are appended in node order: this one ends where the next one starts.
        let start = self.nodes[id as usize].name_off as usize;
        let next = self.nodes.get(id as usize + 1);
        let end = next.map_or(self.names.len(), |n| n.name_off as usize);
        OsStr::from_bytes(&self.names[start..end])
    }

    pub fn children(&self, id: NodeId) -> Range<NodeId> {
        match self.folder(id) {
            Some(f) => {
                let f = &self.folders[f];
                f.first_child..f.first_child + f.n_children
            }
            None => 0..0,
        }
    }

    /// How many files and folders are under `id` (none under a file).
    pub fn items(&self, id: NodeId) -> u32 {
        self.folder(id).map_or(0, |f| self.folders[f].items)
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        (id != 0).then(|| self.nodes[id as usize].parent)
    }

    pub fn path(&self, mut id: NodeId) -> PathBuf {
        let mut names = Vec::new();
        while id != 0 {
            names.push(self.name(id));
            id = self.nodes[id as usize].parent;
        }
        let mut p = self.root_path.clone();
        p.extend(names.iter().rev());
        p
    }

    /// The node at `path` (absolute, under the root, or relative to the root).
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let rel = path.strip_prefix(&self.root_path).unwrap_or(path);
        let mut id = 0;
        for c in rel.components() {
            let Component::Normal(name) = c else {
                return None;
            };
            id = self.children(id).find(|&k| self.name(k) == name)?;
        }
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_is_40_bytes() {
        assert_eq!(std::mem::size_of::<Node>(), 40);
    }

    #[test]
    fn names_and_folders_are_found_across_many_nodes() {
        // The root holds 150 entries, every third a folder holding one file: names of every
        // length up to 255 bytes, folders counted across several words of 64 nodes.
        let name = |i: u32| vec![b'a' + (i % 26) as u8; 1 + i as usize % 255];
        let mut t = Tree::new(PathBuf::from("/r"), 1);
        t.push(0, b"", 0, 0, 1, 0, DIR);
        t.add_children(0, 1, 150);
        for i in 1..=150 {
            let kind = if i % 3 == 0 { DIR } else { FILE };
            t.push(0, &name(i), 10, 10, u64::from(i) + 1, 0, kind);
        }
        // Each folder's one file, pushed in the folders' order: nodes 151 on.
        for (next, i) in (151..).zip((3..=150).step_by(3)) {
            t.add_children(i, next, 1);
            t.push(i, &name(next), 5, 5, u64::from(next) + 1, 0, FILE);
        }
        t.finish();
        for id in 1..t.len() as NodeId {
            assert_eq!(t.name(id).as_bytes(), name(id), "{id}");
        }
        for i in 1..=150 {
            let held = u32::from(i % 3 == 0);
            assert_eq!(
                (t.children(i).len() as u32, t.items(i)),
                (held, held),
                "{i}"
            );
        }
        assert_eq!((t.items(0), t.node(0).alloc), (200, 150 * 10 + 50 * 5));
        let last = t.len() as NodeId - 1;
        assert_eq!(t.parent(last), Some(150));
        assert_eq!(t.find(&t.path(last)), Some(last));
    }

    #[test]
    fn times_before_1970_and_after_2106_are_kept_at_the_ends() {
        let mut t = Tree::new(PathBuf::from("/r"), 1);
        t.push(0, b"", 0, 0, 1, -86_400, DIR);
        t.add_children(0, 1, 2);
        t.push(0, b"old", 0, 0, 2, 1_700_000_000, FILE);
        t.push(0, b"far", 0, 0, 3, 1 << 40, FILE);
        let times: Vec<u32> = (0..3).map(|i| t.node(i).mtime).collect();
        assert_eq!(times, [0, 1_700_000_000, u32::MAX]);
    }

    /// /r (10) holding a/ (10, holding g 1000 and h 1) and f (100).
    fn small() -> Tree {
        small_with(FILE)
    }

    /// `small`, g with these flags.
    fn small_with(g: u16) -> Tree {
        let mut t = Tree::new(PathBuf::from("/r"), 1);
        t.push(0, b"", 10, 10, 1, 0, DIR);
        t.add_children(0, 1, 2);
        t.push(0, b"a", 10, 10, 2, 0, DIR);
        t.push(0, b"f", 100, 100, 3, 0, FILE);
        t.add_children(1, 3, 2);
        t.push(1, b"g", 1000, 1000, 4, 0, g);
        t.push(1, b"h", 1, 1, 5, 0, FILE);
        t.finish();
        t
    }

    fn with(t: &Tree, flag: u16) -> Vec<NodeId> {
        (0..t.len() as NodeId)
            .filter(|&i| t.node(i).flags & flag != 0)
            .collect()
    }

    #[test]
    fn protected_marks_itself_and_a_subtree_and_its_folders_hold_it() {
        let mut t = small();
        assert_eq!((t.node(0).alloc, t.items(0)), (1121, 4));
        t.mark_protected(1, false);
        assert_eq!(with(&t, PROTECTED), [1]);
        assert_eq!(with(&t, HOLDS), [0]);
        t.mark_protected(1, true);
        assert_eq!(with(&t, PROTECTED), [1, 3, 4]);
        assert_eq!(with(&t, HOLDS), [0]);
        // A protected file deeper down: both folders above it hold it.
        let mut t = small();
        t.mark_protected(3, false);
        assert_eq!(
            (with(&t, PROTECTED), with(&t, HOLDS)),
            (vec![3], vec![0, 1])
        );
    }

    #[test]
    fn a_lock_stays_on_its_item_and_the_folders_above_hold_it() {
        for flag in [NOT_OWNED, UNREADABLE, MOUNT] {
            let t = small_with(FILE | flag);
            assert_eq!(with(&t, flag), [3], "{flag:#x}");
            assert_eq!(with(&t, HOLDS), [0, 1], "{flag:#x}");
            assert_eq!(with(&t, LOCKED), [3], "{flag:#x}");
        }
        assert_eq!(with(&small(), HOLDS | LOCKED), Vec::<NodeId>::new());
    }

    #[test]
    fn trashed_leaves_its_folders_totals() {
        let mut t = small();
        t.mark_trashed(3);
        assert_eq!(with(&t, TRASHED), [3]);
        assert_eq!((t.node(1).alloc, t.items(1)), (11, 1));
        assert_eq!(
            (t.node(0).alloc, t.node(0).apparent, t.items(0)),
            (121, 121, 3)
        );
        assert_eq!(t.node(2).alloc, 100);
        // Again, or under a marked folder: nothing more comes off.
        t.mark_trashed(3);
        t.mark_trashed(1);
        t.mark_trashed(4);
        assert_eq!((t.node(0).alloc, t.items(0)), (110, 1));
        assert_eq!((t.node(1).alloc, t.items(1)), (11, 1));
    }
}
