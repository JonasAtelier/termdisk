//! The scan result: every file and folder under the scan root, in one arena.
//!
//! A folder's children sit next to each other in `nodes` (the scanner adds a whole folder at
//! once), so a node needs no sibling links; a child always comes after its parent, which lets
//! `finish` add sizes up in one backward pass. Names live in one shared byte buffer.

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
/// It, or something under it, is not owned by the user the scan ran as.
pub const NOT_OWNED: u16 = 1 << 2;
/// It, or a folder under it, could not be read completely.
pub const UNREADABLE: u16 = 1 << 3;
/// It is, or something under it is, a mount point: the root of another filesystem or of a
/// bind mount. The scan lists a mount point with its own lstat size (as `find -xdev` counts
/// it; uutils `du -x` 0.10 leaves it out) but does not enter it, so it has no children.
pub const MOUNT: u16 = 1 << 4;
/// A file with other hard links, this one carrying the size: trashing it frees nothing while
/// another link remains.
pub const LINKED: u16 = 1 << 5;
/// A hard link to a file already counted elsewhere in the tree: shown, not added to totals.
pub const HARDLINK_DUP: u16 = 1 << 6;
/// It is, contains or lies inside a protected path (`Policy::mark`). A hint for the view only:
/// the trash guard checks everything again when asked to act.
pub const PROTECTED: u16 = 1 << 7;
/// Moved to the trash; no longer counted in its folders' totals.
pub const TRASHED: u16 = 1 << 8;
/// The flags a folder takes from what is under it.
const UP: u16 = NOT_OWNED | UNREADABLE | MOUNT | PROTECTED;

#[derive(Clone, Copy, Debug)]
pub struct Node {
    /// Disk usage in bytes (`st_blocks * 512`); a folder's includes everything under it.
    pub alloc: u64,
    /// Apparent size in bytes (`st_size`); a folder's includes everything under it.
    pub apparent: u64,
    pub ino: u64,
    /// Modification time, seconds since the epoch.
    pub mtime: i64,
    name_off: u32,
    pub parent: NodeId,
    first_child: NodeId,
    n_children: u32,
    /// How many files and folders are under it.
    pub items: u32,
    name_len: u8,
    pub flags: u16,
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
            root_path,
            dev,
            errors: 0,
        }
    }

    /// Records that `parent`'s `n` children are the nodes from `first` on (pushed next).
    pub(crate) fn add_children(&mut self, parent: NodeId, first: NodeId, n: u32) {
        let p = &mut self.nodes[parent as usize];
        p.first_child = first;
        p.n_children = n;
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
        self.nodes.push(Node {
            alloc,
            apparent,
            ino,
            mtime,
            name_off,
            parent,
            first_child: 0,
            n_children: 0,
            items: 0,
            name_len: name.len() as u8,
            flags,
        });
        self.names.extend_from_slice(name);
        id
    }

    pub(crate) fn set_flags(&mut self, id: NodeId, flags: u16) {
        self.nodes[id as usize].flags |= flags;
    }

    /// Marks `id` `PROTECTED`, with its ancestors (they contain it) and, for a protected
    /// subtree, everything under it.
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
            self.nodes[p as usize].flags |= PROTECTED;
            up = p;
        }
    }

    /// Marks `id` `TRASHED` and takes what it counted out of its ancestors' totals. The flags
    /// they took from it stay (a hint that may now be too careful, never too lax).
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
            p_node.items -= 1 + n.items;
            up = p;
        }
    }

    /// Adds every node's sizes, item count and `UP` flags into its parent's, then gives back
    /// the spare capacity.
    pub(crate) fn finish(&mut self) {
        for i in (1..self.nodes.len()).rev() {
            let n = self.nodes[i];
            let p = &mut self.nodes[n.parent as usize];
            if n.flags & HARDLINK_DUP == 0 {
                p.alloc += n.alloc;
                p.apparent += n.apparent;
            }
            p.items += 1 + n.items;
            p.flags |= n.flags & UP;
        }
        self.nodes.shrink_to_fit();
        self.names.shrink_to_fit();
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
        self.nodes.capacity() * std::mem::size_of::<Node>() + self.names.capacity()
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// The node's name; the root's is its whole path.
    pub fn name(&self, id: NodeId) -> &OsStr {
        if id == 0 {
            return self.root_path.as_os_str();
        }
        let n = &self.nodes[id as usize];
        let off = n.name_off as usize;
        OsStr::from_bytes(&self.names[off..off + n.name_len as usize])
    }

    pub fn children(&self, id: NodeId) -> Range<NodeId> {
        let n = &self.nodes[id as usize];
        n.first_child..n.first_child + n.n_children
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
    fn node_is_56_bytes() {
        assert_eq!(std::mem::size_of::<Node>(), 56);
    }

    /// /r (10) holding a/ (10, holding g 1000 and h 1) and f (100).
    fn small() -> Tree {
        let mut t = Tree::new(PathBuf::from("/r"), 1);
        t.push(0, b"", 10, 10, 1, 0, DIR);
        t.add_children(0, 1, 2);
        t.push(0, b"a", 10, 10, 2, 0, DIR);
        t.push(0, b"f", 100, 100, 3, 0, FILE);
        t.add_children(1, 3, 2);
        t.push(1, b"g", 1000, 1000, 4, 0, FILE);
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
    fn protected_goes_up_always_and_down_for_a_subtree() {
        let mut t = small();
        assert_eq!((t.node(0).alloc, t.node(0).items), (1121, 4));
        t.mark_protected(1, false);
        assert_eq!(with(&t, PROTECTED), [0, 1]);
        t.mark_protected(1, true);
        assert_eq!(with(&t, PROTECTED), [0, 1, 3, 4]);
    }

    #[test]
    fn trashed_leaves_its_folders_totals() {
        let mut t = small();
        t.mark_trashed(3);
        assert_eq!(with(&t, TRASHED), [3]);
        assert_eq!((t.node(1).alloc, t.node(1).items), (11, 1));
        assert_eq!(
            (t.node(0).alloc, t.node(0).apparent, t.node(0).items),
            (121, 121, 3)
        );
        assert_eq!(t.node(2).alloc, 100);
        // Again, or under a marked folder: nothing more comes off.
        t.mark_trashed(3);
        t.mark_trashed(1);
        t.mark_trashed(4);
        assert_eq!((t.node(0).alloc, t.node(0).items), (110, 1));
        assert_eq!((t.node(1).alloc, t.node(1).items), (11, 1));
    }
}
