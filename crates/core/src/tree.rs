//! Flat arena tree: nodes live in one `Vec`, names in one shared `String`,
//! children in a CSR (compressed sparse row) index sorted by size descending.

use rayon::prelude::*;

/// Folders with at least this many children are sorted by gathered keys rather than through the node array.
const KEYED_SORT_MIN: usize = 256;
/// ... and in parallel from this many on.
const PAR_SORT_MIN: usize = 32 * 1024;

pub type NodeId = u32;
pub const ROOT: NodeId = 0;
pub const NO_PARENT: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Dir = 1,
    /// Symlink / junction / other directory reparse point: listed, never followed, size 0.
    Link = 2,
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub parent: u32,
    pub name_off: u32,
    /// Logical size. For directories: sum over the subtree (filled by `Tree::from_parts`).
    pub size: u64,
    /// Size on disk (allocated). Same aggregation rules as `size`.
    pub alloc: u64,
    /// Number of regular files in the subtree (1 for a file).
    pub files: u32,
    /// Last modification, Unix seconds (0 = unknown). For directories: the latest one in the subtree.
    pub mtime: u32,
    pub name_len: u16,
    pub kind: Kind,
    /// Deleted after the scan (see [`Tree::remove`]): no longer listed and not counted anywhere.
    pub removed: bool,
}

impl Node {
    pub fn new(parent: u32, name_off: u32, name_len: u16, kind: Kind, size: u64, alloc: u64) -> Self {
        Node {
            parent,
            name_off,
            size,
            alloc,
            files: 0,
            mtime: 0,
            name_len,
            kind,
            removed: false,
        }
    }
}

pub struct Tree {
    nodes: Vec<Node>,
    names: String,
    child_start: Vec<u32>,
    /// Number of live children; `remove` shrinks it, so a node's children are
    /// `children[child_start[i]..child_start[i] + child_len[i]]`.
    child_len: Vec<u32>,
    children: Vec<u32>,
    dir_count: u32,
}

impl Tree {
    /// Builds the tree from raw nodes. `nodes[0]` is the root (`parent == NO_PARENT`).
    /// Node order is arbitrary; sizes of directories are aggregated here.
    /// Nodes that are not reachable from the root (parent missing or cycles) are attached to the root.
    pub fn from_parts(mut nodes: Vec<Node>, names: String) -> Tree {
        assert!(!nodes.is_empty(), "tree needs a root");
        nodes[0].parent = NO_PARENT;
        if Self::is_topological(&nodes) {
            Self::from_topological(nodes, names)
        } else {
            Self::from_any_order(nodes, names)
        }
    }

    /// General path: breadth-first order from the root; unreachable nodes are re-attached to the root.
    fn from_any_order(mut nodes: Vec<Node>, names: String) -> Tree {
        let n = nodes.len();
        let (mut child_start, mut children, mut order) = Self::index(&nodes);
        if order.len() < n {
            let mut seen = vec![false; n];
            for &id in &order {
                seen[id as usize] = true;
            }
            for (i, nd) in nodes.iter_mut().enumerate() {
                if !seen[i] {
                    nd.parent = ROOT;
                }
            }
            (child_start, children, order) = Self::index(&nodes);
            debug_assert_eq!(order.len(), n);
        }

        let dir_count = Self::reset_totals(&mut nodes);
        for &id in order.iter().rev() {
            Self::add_to_parent(&mut nodes, id as usize);
        }
        Self::finish(nodes, names, child_start, children, dir_count)
    }

    /// Every node's parent precedes it (`parent < id`), as the directory walker numbers them: then every node
    /// is reachable from the root, there are no cycles, and descending ids are a valid bottom-up order.
    fn is_topological(nodes: &[Node]) -> bool {
        nodes
            .par_iter()
            .enumerate()
            .skip(1)
            .with_min_len(64 * 1024)
            .all(|(i, nd)| (nd.parent as usize) < i)
    }

    /// Same result as the general path without the breadth-first order: one descending sweep over the node
    /// array aggregates bottom-up and reads memory sequentially.
    fn from_topological(mut nodes: Vec<Node>, names: String) -> Tree {
        let (child_start, children) = Self::child_index(&nodes);
        let dir_count = Self::reset_totals(&mut nodes);
        for id in (1..nodes.len()).rev() {
            Self::add_to_parent(&mut nodes, id);
        }
        Self::finish(nodes, names, child_start, children, dir_count)
    }

    /// Clears directory/link totals before aggregation; returns the number of directories.
    fn reset_totals(nodes: &mut [Node]) -> u32 {
        let mut dir_count = 0u32;
        for nd in nodes.iter_mut() {
            match nd.kind {
                Kind::File => nd.files = 1,
                Kind::Dir => {
                    dir_count += 1;
                    nd.size = 0;
                    nd.alloc = 0;
                    nd.files = 0;
                }
                Kind::Link => {
                    nd.size = 0;
                    nd.alloc = 0;
                    nd.files = 0;
                }
            }
        }
        dir_count
    }

    fn add_to_parent(nodes: &mut [Node], id: usize) {
        let nd = nodes[id];
        if nd.parent != NO_PARENT {
            let p = &mut nodes[nd.parent as usize];
            // Wrapping: a corrupt record may claim an absurd size; that must not panic (debug builds). Real volumes
            // never get near 2^64 bytes, and `remove` subtracts the same way, so the totals stay consistent.
            p.size = p.size.wrapping_add(nd.size);
            p.alloc = p.alloc.wrapping_add(nd.alloc);
            p.files = p.files.wrapping_add(nd.files);
            if nd.kind != Kind::Link {
                p.mtime = p.mtime.max(nd.mtime);
            }
        }
    }

    fn finish(nodes: Vec<Node>, names: String, child_start: Vec<u32>, mut children: Vec<u32>, dir_count: u32) -> Tree {
        Self::sort_children(&nodes, &child_start, &mut children);
        let child_len = child_start.windows(2).map(|w| w[1] - w[0]).collect();
        Tree {
            nodes,
            names,
            child_start,
            child_len,
            children,
            dir_count,
        }
    }

    fn index(nodes: &[Node]) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        let n = nodes.len();
        let (child_start, children) = Self::child_index(nodes);
        let mut order = Vec::with_capacity(n);
        order.push(ROOT);
        let mut head = 0;
        while head < order.len() {
            let id = order[head] as usize;
            head += 1;
            let (s, e) = (child_start[id] as usize, child_start[id + 1] as usize);
            order.extend_from_slice(&children[s..e]);
        }
        (child_start, children, order)
    }

    /// CSR index: the children of `i` are `children[child_start[i]..child_start[i + 1]]`, ascending ids.
    fn child_index(nodes: &[Node]) -> (Vec<u32>, Vec<u32>) {
        let n = nodes.len();
        let mut child_start = vec![0u32; n + 1];
        for (i, nd) in nodes.iter().enumerate() {
            if i != 0 && nd.parent != NO_PARENT {
                child_start[nd.parent as usize + 1] += 1;
            }
        }
        for i in 0..n {
            child_start[i + 1] += child_start[i];
        }
        let mut cursor = child_start.clone();
        let mut children = vec![0u32; child_start[n] as usize];
        for (i, nd) in nodes.iter().enumerate() {
            if i != 0 && nd.parent != NO_PARENT {
                let c = &mut cursor[nd.parent as usize];
                children[*c as usize] = i as u32;
                *c += 1;
            }
        }
        (child_start, children)
    }

    fn sort_children(nodes: &[Node], child_start: &[u32], children: &mut [u32]) {
        let mut slices: Vec<&mut [u32]> = Vec::new();
        let mut rest = children;
        for w in child_start.windows(2) {
            let (head, tail) = rest.split_at_mut((w[1] - w[0]) as usize);
            if head.len() > 1 {
                slices.push(head);
            }
            rest = tail;
        }
        slices.par_iter_mut().for_each(|s| {
            if s.len() < KEYED_SORT_MIN {
                s.sort_unstable_by(|&a, &b| nodes[b as usize].size.cmp(&nodes[a as usize].size).then(a.cmp(&b)));
                return;
            }
            // Big folders: gather the keys once (the comparisons then stay in cache instead of chasing
            // `nodes[..]` for every compare) and sort them in parallel. Order: size descending, then id.
            let mut keyed: Vec<(u64, u32)> = s.iter().map(|&c| (u64::MAX - nodes[c as usize].size, c)).collect();
            if keyed.len() >= PAR_SORT_MIN {
                keyed.par_sort_unstable();
            } else {
                keyed.sort_unstable();
            }
            for (slot, (_, c)) in s.iter_mut().zip(keyed) {
                *slot = c;
            }
        });
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn name(&self, id: NodeId) -> &str {
        let n = &self.nodes[id as usize];
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.nodes[id as usize].parent;
        (p != NO_PARENT).then_some(p)
    }

    /// Children sorted by size descending.
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        let s = self.child_start[id as usize] as usize;
        &self.children[s..s + self.child_len[id as usize] as usize]
    }

    fn children_mut(&mut self, id: NodeId) -> &mut [NodeId] {
        let s = self.child_start[id as usize] as usize;
        &mut self.children[s..s + self.child_len[id as usize] as usize]
    }

    pub fn is_removed(&self, id: NodeId) -> bool {
        self.nodes[id as usize].removed
    }

    /// Takes a deleted file or folder out of the tree: its subtree is marked removed, ancestors lose its
    /// size / allocation / file count and are re-sorted among their siblings. Returns false for the root
    /// or an already removed node.
    pub fn remove(&mut self, id: NodeId) -> bool {
        if id == ROOT || self.nodes[id as usize].removed {
            return false;
        }
        let Node { size, alloc, files, .. } = self.nodes[id as usize];
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let nd = &mut self.nodes[n as usize];
            nd.removed = true;
            if nd.kind == Kind::Dir {
                self.dir_count -= 1;
            }
            stack.extend_from_slice(self.children(n));
        }

        let parent = self.nodes[id as usize].parent;
        let siblings = self.children_mut(parent);
        if let Some(pos) = siblings.iter().position(|&c| c == id) {
            siblings[pos..].rotate_left(1);
            self.child_len[parent as usize] -= 1;
        }
        let mut cur = parent;
        loop {
            let nd = &mut self.nodes[cur as usize];
            // The exact inverse of the wrapping sums in `add_to_parent`.
            nd.size = nd.size.wrapping_sub(size);
            nd.alloc = nd.alloc.wrapping_sub(alloc);
            nd.files = nd.files.wrapping_sub(files);
            let Some(p) = self.parent(cur) else { break };
            // `cur` shrank: move it right past siblings that are now larger (order: size desc, then id).
            let nodes = &self.nodes;
            let key = |c: NodeId| (std::cmp::Reverse(nodes[c as usize].size), c);
            let s = self.child_start[p as usize] as usize;
            let sib = &mut self.children[s..s + self.child_len[p as usize] as usize];
            if let Some(mut i) = sib.iter().position(|&c| c == cur) {
                while i + 1 < sib.len() && key(sib[i + 1]) < key(sib[i]) {
                    sib.swap(i, i + 1);
                    i += 1;
                }
            }
            cur = p;
        }
        true
    }

    /// Moves `id` to where it belongs among its siblings (size descending, then id) after its size changed.
    fn resort_in_parent(&mut self, id: NodeId) {
        let Some(p) = self.parent(id) else { return };
        let nodes = &self.nodes;
        let key = |c: NodeId| (std::cmp::Reverse(nodes[c as usize].size), c);
        let s = self.child_start[p as usize] as usize;
        let sib = &mut self.children[s..s + self.child_len[p as usize] as usize];
        let Some(mut i) = sib.iter().position(|&c| c == id) else {
            return;
        };
        while i > 0 && key(sib[i - 1]) > key(sib[i]) {
            sib.swap(i, i - 1);
            i -= 1;
        }
        while i + 1 < sib.len() && key(sib[i + 1]) < key(sib[i]) {
            sib.swap(i, i + 1);
            i += 1;
        }
    }

    /// Adds `(size, alloc, files)` (wrapping, as everywhere else) to every ancestor starting at `from`, keeping
    /// each of them in order among its siblings and `mtime` as the latest of the subtree.
    fn bump_ancestors(&mut self, from: NodeId, size: u64, alloc: u64, files: u32, mtime: u32) {
        let mut cur = from;
        loop {
            let nd = &mut self.nodes[cur as usize];
            nd.size = nd.size.wrapping_add(size);
            nd.alloc = nd.alloc.wrapping_add(alloc);
            nd.files = nd.files.wrapping_add(files);
            nd.mtime = nd.mtime.max(mtime);
            self.resort_in_parent(cur);
            let Some(p) = self.parent(cur) else { break };
            cur = p;
        }
    }

    /// A file that was already scanned changed size on disk (live monitoring). Directories and removed nodes are
    /// left alone. Returns false when nothing was changed.
    pub fn resize(&mut self, id: NodeId, size: u64, alloc: u64, mtime: u32) -> bool {
        let nd = self.nodes[id as usize];
        if nd.removed || nd.kind != Kind::File {
            return false;
        }
        let Some(parent) = self.parent(id) else { return false };
        {
            let nd = &mut self.nodes[id as usize];
            nd.size = size;
            nd.alloc = alloc;
            nd.mtime = nd.mtime.max(mtime);
        }
        self.resort_in_parent(id);
        self.bump_ancestors(
            parent,
            size.wrapping_sub(nd.size),
            alloc.wrapping_sub(nd.alloc),
            0,
            mtime,
        );
        true
    }

    /// Gives a node a new name (live monitoring: a rename within the same folder). The old name stays in the shared
    /// name buffer, unreferenced. False for the root, removed nodes, empty or oversized names.
    pub fn rename(&mut self, id: NodeId, name: &str) -> bool {
        let fits = u32::try_from(self.names.len() + name.len()).is_ok() && u16::try_from(name.len()).is_ok();
        if id == ROOT || self.nodes[id as usize].removed || name.is_empty() || !fits {
            return false;
        }
        let off = self.names.len() as u32;
        self.names.push_str(name);
        let nd = &mut self.nodes[id as usize];
        (nd.name_off, nd.name_len) = (off, name.len() as u16);
        true
    }

    /// Adds a file or an (empty) folder that appeared after the scan. `None` when `parent` is not a live folder or
    /// the tree cannot address more names.
    pub fn insert(
        &mut self,
        parent: NodeId,
        name: &str,
        kind: Kind,
        size: u64,
        alloc: u64,
        mtime: u32,
    ) -> Option<NodeId> {
        let p = self.nodes.get(parent as usize)?;
        let fits = u32::try_from(self.names.len() + name.len()).is_ok()
            && u16::try_from(name.len()).is_ok()
            && self.nodes.len() < (u32::MAX - 1) as usize
            && self.children.len() < (u32::MAX - 1) as usize;
        if p.removed || p.kind != Kind::Dir || kind == Kind::Link || name.is_empty() || !fits {
            return None;
        }
        let id = self.nodes.len() as u32;
        let (size, alloc, files) = if kind == Kind::File {
            (size, alloc, 1)
        } else {
            (0, 0, 0)
        };
        let mut node = Node::new(parent, self.names.len() as u32, name.len() as u16, kind, size, alloc);
        (node.files, node.mtime) = (files, mtime);
        self.names.push_str(name);
        self.nodes.push(node);
        // The CSR index has one spare slot (`child_start[len]`); the new node's empty block goes there.
        let end = self.children.len() as u32;
        self.child_start[id as usize] = end;
        self.child_start.push(end);
        self.child_len.push(0);
        // The parent's block is copied to the end unless it already is the last one (a folder that keeps
        // receiving files is moved once, then grows in place).
        let (s, l) = (
            self.child_start[parent as usize] as usize,
            self.child_len[parent as usize] as usize,
        );
        if s + l == self.children.len() {
            self.children.push(id);
        } else {
            let new_start = self.children.len() as u32;
            self.children.extend_from_within(s..s + l);
            self.children.push(id);
            self.child_start[parent as usize] = new_start;
        }
        self.child_len[parent as usize] += 1;
        if kind == Kind::Dir {
            self.dir_count += 1;
        }
        self.resort_in_parent(id);
        self.bump_ancestors(parent, size, alloc, files, mtime);
        Some(id)
    }

    pub fn is_dir(&self, id: NodeId) -> bool {
        self.nodes[id as usize].kind == Kind::Dir
    }

    pub fn dir_count(&self) -> u32 {
        self.dir_count
    }

    pub fn root_stats(&self) -> (u64, u64, u32) {
        let r = &self.nodes[0];
        (r.size, r.alloc, r.files)
    }

    pub fn path(&self, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        loop {
            parts.push(self.name(cur));
            match self.parent(cur) {
                Some(p) => cur = p,
                None => break,
            }
        }
        let mut out = String::new();
        for part in parts.iter().rev() {
            if !out.is_empty() && !out.ends_with(std::path::MAIN_SEPARATOR) {
                out.push(std::path::MAIN_SEPARATOR);
            }
            out.push_str(part);
        }
        out
    }

    /// Ancestors from the root down to (and including) `id`.
    pub fn ancestors(&self, id: NodeId) -> Vec<NodeId> {
        let mut v = vec![id];
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            v.push(p);
            cur = p;
        }
        v.reverse();
        v
    }
}

/// Helper to assemble nodes + shared names buffer.
#[derive(Default)]
pub struct Builder {
    pub nodes: Vec<Node>,
    pub names: String,
}

impl Builder {
    pub fn add(&mut self, parent: u32, name: &str, kind: Kind, size: u64, alloc: u64) -> NodeId {
        let off = self.names.len() as u32;
        self.names.push_str(name);
        let id = self.nodes.len() as u32;
        self.nodes
            .push(Node::new(parent, off, name.len() as u16, kind, size, alloc));
        id
    }

    pub fn finish(self) -> Tree {
        Tree::from_parts(self.nodes, self.names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_and_sorts_regardless_of_node_order() {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "root", Kind::Dir, 0, 0);
        // child created before its parent directory (arbitrary order, as in MFT)
        let f1 = b.add(2, "a.txt", Kind::File, 10, 4096);
        let d = b.add(root, "d", Kind::Dir, 0, 0);
        let f2 = b.add(root, "big.bin", Kind::File, 100, 4096);
        let f3 = b.add(d, "b.txt", Kind::File, 5, 4096);
        let lnk = b.add(root, "lnk", Kind::Link, 77, 77);
        let t = b.finish();
        assert_eq!(t.root_stats(), (115, 3 * 4096, 3));
        assert_eq!(t.node(d).size, 15);
        assert_eq!(t.node(d).files, 2);
        assert_eq!(t.children(root), &[f2, d, lnk]);
        assert_eq!(t.children(d), &[f1, f3]);
        assert_eq!(t.path(f3), format!("root{0}d{0}b.txt", std::path::MAIN_SEPARATOR));
    }

    /// Directory totals equal the sum of the live children, siblings are in order, every live node is listed.
    fn check_invariants(t: &Tree) {
        let mut listed = vec![false; t.len()];
        listed[ROOT as usize] = true;
        for id in 0..t.len() as NodeId {
            if t.is_removed(id) {
                continue;
            }
            let ch = t.children(id);
            for w in ch.windows(2) {
                let k = |c: NodeId| (std::cmp::Reverse(t.node(c).size), c);
                assert!(k(w[0]) < k(w[1]), "children of {id} out of order");
            }
            for &c in ch {
                assert!(!t.is_removed(c) && t.parent(c) == Some(id));
                listed[c as usize] = true;
            }
            if t.is_dir(id) {
                let sum = |f: fn(&Node) -> u64| ch.iter().map(|&c| f(t.node(c))).sum::<u64>();
                assert_eq!(t.node(id).size, sum(|n| n.size), "size of {id}");
                assert_eq!(t.node(id).alloc, sum(|n| n.alloc), "alloc of {id}");
                assert_eq!(
                    u64::from(t.node(id).files),
                    sum(|n| u64::from(n.files)),
                    "files of {id}"
                );
            }
        }
        for id in 0..t.len() as NodeId {
            assert_eq!(listed[id as usize], !t.is_removed(id), "node {id} listing");
        }
    }

    #[test]
    fn live_changes_keep_totals_and_order() {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "r", Kind::Dir, 0, 0);
        let mut dirs = vec![root];
        for i in 0..6 {
            dirs.push(b.add(dirs[i / 2], &format!("d{i}"), Kind::Dir, 0, 0));
        }
        let mut files = Vec::new();
        for i in 0..40u64 {
            files.push(b.add(
                dirs[i as usize % dirs.len()],
                &format!("f{i}"),
                Kind::File,
                i * 7 % 23,
                i * 7 % 23,
            ));
        }
        let mut t = b.finish();
        check_invariants(&t);

        let mut seed = 12345u64;
        let mut rnd = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for step in 0..400 {
            match rnd() % 4 {
                0 | 1 => {
                    let parent = dirs[rnd() % dirs.len()];
                    if let Some(id) = t.insert(parent, &format!("n{step}"), Kind::File, (rnd() % 100) as u64, 4096, 5) {
                        files.push(id);
                    }
                }
                2 => {
                    let f = files[rnd() % files.len()];
                    t.resize(f, (rnd() % 200) as u64, 4096, 9);
                }
                _ => {
                    let f = files[rnd() % files.len()];
                    t.remove(f);
                }
            }
            if step % 20 == 0 {
                let id = t
                    .insert(dirs[rnd() % dirs.len()], &format!("nd{step}"), Kind::Dir, 0, 0, 0)
                    .unwrap();
                dirs.push(id);
            }
            check_invariants(&t);
        }
    }

    #[test]
    fn rename_changes_only_the_name() {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "r", Kind::Dir, 0, 0);
        let f = b.add(root, "old.txt", Kind::File, 5, 5);
        let mut t = b.finish();
        assert!(t.rename(f, "ново.txt"));
        assert_eq!(t.name(f), "ново.txt");
        assert_eq!(t.node(f).size, 5);
        assert_eq!(t.children(root), &[f]);
        assert!(!t.rename(f, "") && !t.rename(root, "x"));
        t.remove(f);
        assert!(!t.rename(f, "again"));
    }

    #[test]
    fn insert_rejects_bad_parents() {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "r", Kind::Dir, 0, 0);
        let f = b.add(root, "f", Kind::File, 1, 1);
        let mut t = b.finish();
        assert!(
            t.insert(f, "x", Kind::File, 1, 1, 0).is_none(),
            "a file is not a parent"
        );
        assert!(t.insert(root, "", Kind::File, 1, 1, 0).is_none());
        assert!(!t.resize(root, 5, 5, 0), "folders are not resized directly");
        t.remove(f);
        assert!(t.insert(f, "x", Kind::File, 1, 1, 0).is_none());
        assert!(!t.resize(f, 9, 9, 0), "removed");
    }

    #[test]
    fn remove_updates_ancestors_order_and_counts() {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "r", Kind::Dir, 0, 0);
        let a = b.add(root, "a", Kind::Dir, 0, 0);
        let big = b.add(a, "big", Kind::File, 100, 100);
        let small = b.add(a, "small", Kind::File, 5, 5);
        let c = b.add(root, "c", Kind::Dir, 0, 0);
        let inner = b.add(c, "inner", Kind::Dir, 0, 0);
        b.add(inner, "f", Kind::File, 50, 50);
        let mut t = b.finish();
        assert_eq!(t.children(root), &[a, c]);
        assert_eq!(t.dir_count(), 4);

        assert!(t.remove(big));
        assert_eq!(t.children(a), &[small]);
        assert_eq!(t.node(a).size, 5);
        assert_eq!(t.children(root), &[c, a], "a shrank below c and must move after it");
        assert_eq!(t.root_stats(), (55, 55, 2));

        assert!(t.remove(inner));
        assert!(t.is_removed(inner) && t.children(c).is_empty());
        assert_eq!(t.root_stats(), (5, 5, 1));
        assert_eq!(t.dir_count(), 3);
        assert!(!t.remove(inner), "already removed");
        assert!(!t.remove(root), "the root cannot be removed");
    }

    /// Random tree in walker order (`parent < id`): dirs, files and links, sizes with many ties.
    fn random_nodes(seed: u64, n: usize) -> (Vec<Node>, String) {
        let mut s = seed;
        let mut rnd = move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            s >> 33
        };
        let mut b = Builder::default();
        b.add(NO_PARENT, "r", Kind::Dir, 0, 0);
        let mut dirs = vec![0u32];
        for i in 1..n {
            let parent = dirs[rnd() as usize % dirs.len()];
            let kind = match rnd() % 10 {
                0..=2 => Kind::Dir,
                3 => Kind::Link,
                _ => Kind::File,
            };
            let id = b.add(parent, &format!("n{i}"), kind, rnd() % 50, rnd() % 50 * 4096);
            b.nodes[id as usize].mtime = (rnd() % 1000) as u32;
            if kind == Kind::Dir {
                dirs.push(id);
            }
        }
        (b.nodes, b.names)
    }

    fn assert_same(a: &Tree, b: &Tree) {
        assert_eq!(a.len(), b.len());
        assert_eq!(a.dir_count(), b.dir_count());
        for id in 0..a.len() as NodeId {
            let (x, y) = (a.node(id), b.node(id));
            assert_eq!(
                (x.parent, x.size, x.alloc, x.files, x.mtime, x.kind),
                (y.parent, y.size, y.alloc, y.files, y.mtime, y.kind),
                "node {id}"
            );
            assert_eq!(a.children(id), b.children(id), "children of {id}");
            assert_eq!(a.name(id), b.name(id));
        }
    }

    #[test]
    fn differential_topological_fast_path_matches_general_path() {
        for (seed, n) in [(1, 2), (2, 50), (3, 5_000), (4, 70_000)] {
            let (nodes, names) = random_nodes(seed, n);
            assert!(Tree::is_topological(&nodes));
            let fast = Tree::from_parts(nodes.clone(), names.clone());
            let general = Tree::from_any_order(nodes, names);
            assert_same(&fast, &general);
        }
    }

    #[test]
    fn differential_shuffled_ids_give_the_same_tree() {
        let (nodes, names) = random_nodes(9, 20_000);
        let reference = Tree::from_parts(nodes.clone(), names.clone());
        // Reverse ids 1.. (the root stays 0): almost every parent now follows its child.
        let n = nodes.len() as u32;
        let map = |id: u32| if id == 0 || id == NO_PARENT { id } else { n - id };
        let mut shuffled = nodes.clone();
        for (i, nd) in nodes.iter().enumerate() {
            let mut m = *nd;
            m.parent = map(nd.parent);
            shuffled[map(i as u32) as usize] = m;
        }
        assert!(!Tree::is_topological(&shuffled));
        let t = Tree::from_parts(shuffled, names);
        for id in 0..n {
            let (x, y) = (reference.node(id), t.node(map(id)));
            assert_eq!((x.size, x.alloc, x.files, x.mtime), (y.size, y.alloc, y.files, y.mtime));
            let mut a: Vec<u32> = reference.children(id).iter().map(|&c| map(c)).collect();
            let mut b = t.children(map(id)).to_vec();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b);
        }
    }

    /// Corrupt MFT records can claim sizes near `u64::MAX`; building and editing the tree must not panic.
    #[test]
    fn huge_sizes_do_not_panic_and_removal_stays_exact() {
        let half = u64::MAX / 2 + 1;
        for topological in [true, false] {
            let mut b = Builder::default();
            let root = b.add(NO_PARENT, "r", Kind::Dir, 0, 0);
            let (d, x, y) = if topological {
                let d = b.add(root, "d", Kind::Dir, 0, 0);
                (
                    d,
                    b.add(d, "x", Kind::File, half, half),
                    b.add(d, "y", Kind::File, half, half),
                )
            } else {
                let x = b.add(3, "x", Kind::File, half, half);
                let y = b.add(3, "y", Kind::File, half, half);
                (b.add(root, "d", Kind::Dir, 0, 0), x, y)
            };
            let mut t = b.finish(); // overflowed with a panic in debug builds
            assert_eq!(t.root_stats().2, 2);
            assert!(t.remove(x));
            assert_eq!(t.node(d).size, half, "removal is the exact inverse of the sum");
            assert!(t.remove(y));
            assert_eq!(t.root_stats(), (0, 0, 0));
        }
    }

    #[test]
    fn unreachable_nodes_are_attached_to_root() {
        let mut b = Builder::default();
        b.add(NO_PARENT, "r", Kind::Dir, 0, 0);
        b.add(2, "x", Kind::Dir, 0, 0); // 1 <-> 2 cycle
        b.add(1, "y", Kind::Dir, 0, 0);
        b.add(1, "f", Kind::File, 3, 3);
        let t = b.finish();
        assert_eq!(t.root_stats().0, 3);
        assert!(t.path(3).starts_with('r'));
    }
}
