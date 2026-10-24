//! Live monitoring: keeps the tree in step with the disk through the NTFS change journal while the program is
//! open, so the treemap follows a process that suddenly writes gigabytes of logs or cache.
//!
//! The journal reader (`spacetree_core::usn`) hands over ready-made changes; here they are applied to the tree
//! in small batches, the treemap is re-laid out a few times per second (the tiles glide to their new places) and
//! the lists (largest files, cleanup suggestions) are refreshed every half minute.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use eframe::egui;
use spacetree_core::usn::{self, Change, Monitor, record_of};
use spacetree_core::{Kind, NO_NODE, NodeId, Tree, drive_of_root};

use crate::{App, ToastKind, diag, now_unix, settings};

/// Changes applied per frame at most; the rest wait for the next one so that the window never stalls.
const MAX_PER_FRAME: usize = 4000;
/// Waiting changes beyond this are dropped (and counted): the tree is hopelessly behind and a rescan is due.
const QUEUE_CAP: usize = 200_000;
const LAYOUT_EVERY: Duration = Duration::from_millis(250);
const LISTS_EVERY: Duration = Duration::from_secs(30);
/// How long a changed tile keeps its highlight.
pub const FLASH: Duration = Duration::from_secs(3);

pub struct Live {
    monitor: Option<Monitor>,
    /// Files created after the scan: MFT record number -> node.
    extra: HashMap<u64, NodeId>,
    queue: VecDeque<Change>,
    /// Nodes whose size changed recently and when (the treemap flashes them).
    pub recent: HashMap<NodeId, Instant>,
    /// Changes applied to the tree since monitoring started.
    pub applied: u64,
    /// Changes that could not be placed (unknown folder, moved folder...): the tree may drift from the disk.
    pub unplaced: u64,
    last_layout: Instant,
    last_lists: Instant,
    tree_dirty: bool,
    lists_dirty: bool,
}

impl Default for Live {
    fn default() -> Self {
        Live {
            monitor: None,
            extra: HashMap::new(),
            queue: VecDeque::new(),
            recent: HashMap::new(),
            applied: 0,
            unplaced: 0,
            last_layout: Instant::now(),
            last_lists: Instant::now(),
            tree_dirty: false,
            lists_dirty: false,
        }
    }
}

impl Live {
    pub fn active(&self) -> bool {
        self.monitor.is_some()
    }

    /// `(files seen changing, dropped because of a flood)` since monitoring started.
    pub fn stats(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.monitor.as_ref().map_or((0, 0), |m| {
            (m.stats.events.load(Relaxed), m.stats.dropped.load(Relaxed))
        })
    }
}

/// What applying one change did.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    /// Nothing to do (or nothing known about the file).
    Ignored,
    /// A file changed size.
    Resized(NodeId),
    /// Nodes were added, removed or renamed.
    Structure,
    /// The change could not be placed in the tree.
    Unplaced,
}

fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

/// The live node of an MFT record, if the tree has one (a record number can be reused: callers check the name).
fn lookup(tree: &Tree, record_nodes: &[u32], extra: &HashMap<u64, NodeId>, rec: u64) -> Option<NodeId> {
    let node = extra.get(&rec).copied().or_else(|| {
        record_nodes
            .get(usize::try_from(rec).ok()?)
            .copied()
            .filter(|&n| n != NO_NODE)
    })?;
    (usize::try_from(node).is_ok_and(|n| n < tree.len()) && !tree.is_removed(node)).then_some(node)
}

#[allow(clippy::too_many_arguments)]
fn create(
    tree: &mut Tree,
    record_nodes: &[u32],
    extra: &mut HashMap<u64, NodeId>,
    rec: u64,
    parent_frn: u64,
    name: &str,
    is_dir: bool,
    (size, alloc): (u64, u64),
    now: u32,
) -> Effect {
    let Some(parent) = lookup(tree, record_nodes, extra, record_of(parent_frn)).filter(|&p| tree.is_dir(p)) else {
        return Effect::Unplaced;
    };
    if let Some(old) = lookup(tree, record_nodes, extra, rec) {
        if same_name(tree.name(old), name) && tree.parent(old) == Some(parent) {
            // already known (the scan saw it, or the journal replays what the scan caught)
            return if tree.resize(old, size, alloc, now) {
                Effect::Resized(old)
            } else {
                Effect::Ignored
            };
        }
        tree.remove(old); // the record number was reused by a new file
        extra.remove(&rec);
    }
    let kind = if is_dir { Kind::Dir } else { Kind::File };
    match tree.insert(parent, name, kind, size, alloc, now) {
        Some(id) => {
            extra.insert(rec, id);
            Effect::Structure
        }
        None => Effect::Unplaced,
    }
}

/// Applies one journal change to the tree.
fn apply_change(
    tree: &mut Tree,
    record_nodes: &[u32],
    extra: &mut HashMap<u64, NodeId>,
    change: Change,
    now: u32,
) -> Effect {
    match change {
        Change::Resized { frn, name, size, alloc } => {
            match lookup(tree, record_nodes, extra, record_of(frn)) {
                Some(n) if tree.node(n).kind == Kind::File && same_name(tree.name(n), &name) => {
                    if tree.resize(n, size, alloc, now) {
                        Effect::Resized(n)
                    } else {
                        Effect::Ignored
                    }
                }
                _ => Effect::Ignored, // not in the tree (system file, created before monitoring...)
            }
        }
        Change::Created {
            frn,
            parent_frn,
            name,
            is_dir,
            size,
            alloc,
        } => create(
            tree,
            record_nodes,
            extra,
            record_of(frn),
            parent_frn,
            &name,
            is_dir,
            (size, alloc),
            now,
        ),
        Change::Renamed {
            frn,
            parent_frn,
            name,
            is_dir,
            size,
            alloc,
        } => {
            let rec = record_of(frn);
            let Some(node) = lookup(tree, record_nodes, extra, rec) else {
                // moved here from outside the tree (or unknown): it is new as far as the tree knows
                return create(
                    tree,
                    record_nodes,
                    extra,
                    rec,
                    parent_frn,
                    &name,
                    is_dir,
                    (size, alloc),
                    now,
                );
            };
            let new_parent = lookup(tree, record_nodes, extra, record_of(parent_frn));
            if new_parent.is_some() && tree.parent(node) == new_parent {
                if tree.name(node) == name || tree.rename(node, &name) {
                    Effect::Structure
                } else {
                    Effect::Unplaced
                }
            } else if tree.is_dir(node) {
                Effect::Unplaced // moving a folder with everything in it is not supported: a rescan will show it
            } else {
                tree.remove(node);
                extra.remove(&rec);
                match new_parent {
                    Some(_) => create(
                        tree,
                        record_nodes,
                        extra,
                        rec,
                        parent_frn,
                        &name,
                        is_dir,
                        (size, alloc),
                        now,
                    ),
                    None => Effect::Structure, // moved out of the scanned area
                }
            }
        }
        Change::Deleted { frn, name } => {
            let rec = record_of(frn);
            match lookup(tree, record_nodes, extra, rec) {
                Some(n) if same_name(tree.name(n), &name) => {
                    tree.remove(n);
                    extra.remove(&rec);
                    Effect::Structure
                }
                _ => Effect::Ignored,
            }
        }
        Change::Lost(_) => Effect::Ignored,
    }
}

impl App {
    /// Live monitoring needs an MFT scan of a whole NTFS volume (the journal speaks in MFT record numbers).
    pub fn live_available(&self) -> bool {
        !settings::is_demo()
            && self
                .data
                .as_ref()
                .is_some_and(|d| !d.out.record_nodes.is_empty() && drive_of_root(&d.root_path).is_some())
    }

    pub fn start_live(&mut self, ctx: &egui::Context) {
        if !self.live_available() || self.live.active() {
            return;
        }
        let Some(data) = &self.data else { return };
        let Some(drive) = drive_of_root(&data.root_path) else {
            return;
        };
        let ctx2 = ctx.clone();
        match usn::start(drive, data.out.usn, move || ctx2.request_repaint()) {
            Ok(m) => {
                diag::info(format!("live monitoring started on {drive}:"));
                self.live = Live {
                    monitor: Some(m),
                    ..Live::default()
                };
            }
            Err(e) => {
                diag::warn(format!("live monitoring: {e}"));
                self.toast(format!("Живое отслеживание не включилось: {e}"), ToastKind::Error);
            }
        }
    }

    pub fn stop_live(&mut self) {
        if self.live.active() {
            diag::info("live monitoring stopped");
        }
        self.live = Live::default();
    }

    /// Takes the changes the journal reader has queued and applies a batch of them. Called every frame.
    pub fn poll_live(&mut self, ctx: &egui::Context) {
        let Some(monitor) = &self.live.monitor else { return };
        let mut lost = None;
        while let Ok(c) = monitor.rx.try_recv() {
            match c {
                Change::Lost(why) => lost = Some(why),
                c if self.live.queue.len() < QUEUE_CAP => self.live.queue.push_back(c),
                _ => self.live.unplaced += 1,
            }
        }
        if let Some(why) = lost {
            self.stop_live();
            self.toast(format!("Живое отслеживание остановлено: {why}"), ToastKind::Error);
            return;
        }
        if !self.live.queue.is_empty() {
            // The tree can only be modified when nothing else holds it (a background reader may); try again next frame.
            if let Some(data) = self.data.as_mut().and_then(std::sync::Arc::get_mut) {
                let now = now_unix();
                let started = Instant::now();
                let mut n = 0;
                while n < MAX_PER_FRAME && started.elapsed() < Duration::from_millis(12) {
                    let Some(change) = self.live.queue.pop_front() else {
                        break;
                    };
                    n += 1;
                    let live = &mut self.live;
                    match apply_change(&mut data.out.tree, &data.out.record_nodes, &mut live.extra, change, now) {
                        Effect::Resized(id) => {
                            live.applied += 1;
                            live.tree_dirty = true;
                            live.recent.insert(id, Instant::now());
                        }
                        Effect::Structure => {
                            live.applied += 1;
                            live.tree_dirty = true;
                            live.lists_dirty = true;
                        }
                        Effect::Unplaced => live.unplaced += 1,
                        Effect::Ignored => {}
                    }
                }
            }
            ctx.request_repaint();
        }
        if self.live.tree_dirty && self.live.last_layout.elapsed() >= LAYOUT_EVERY {
            self.live.tree_dirty = false;
            self.live.last_layout = Instant::now();
            self.live_tree_changed();
        }
        if self.live.lists_dirty
            && self.live.last_lists.elapsed() >= LISTS_EVERY
            && let Some(data) = self.data.as_mut().and_then(std::sync::Arc::get_mut)
        {
            self.live.lists_dirty = false;
            self.live.last_lists = Instant::now();
            data.reanalyze();
            data.index_streams();
            self.top_order_key = None;
            self.marked.retain(|&m| !data.out.tree.is_removed(m));
        }
        self.live.recent.retain(|_, t| t.elapsed() < FLASH);
        if self.live.tree_dirty || !self.live.queue.is_empty() || !self.live.recent.is_empty() {
            ctx.request_repaint_after(LAYOUT_EVERY);
        }
    }

    /// The tree changed under the user's hands: refresh what is on screen without losing the position.
    fn live_tree_changed(&mut self) {
        self.generation += 1;
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        if self.selected.is_some_and(|s| tree.is_removed(s)) {
            self.selected = None;
        }
        while tree.is_removed(self.tm_root) {
            self.tm_root = tree.parent(self.tm_root).unwrap_or(spacetree_core::ROOT);
        }
        self.expanded.retain(|&e| !tree.is_removed(e));
        self.rows_dirty = true;
        self.search_done.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spacetree_core::tree::{Builder, NO_PARENT};

    const SEQ: u64 = 7 << 48; // sequence numbers must not matter

    /// Record numbers: 5 root, 16 `docs`, 17 `docs\a.txt` (100 B), 18 `b.bin` (50 B) in the root.
    fn fixture() -> (Tree, Vec<u32>) {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, r"C:\", Kind::Dir, 0, 0);
        let docs = b.add(root, "docs", Kind::Dir, 0, 0);
        let a = b.add(docs, "a.txt", Kind::File, 100, 4096);
        let bin = b.add(root, "b.bin", Kind::File, 50, 4096);
        let mut rn = vec![NO_NODE; 32];
        (rn[5], rn[16], rn[17], rn[18]) = (root, docs, a, bin);
        (b.finish(), rn)
    }

    fn run(tree: &mut Tree, rn: &[u32], extra: &mut HashMap<u64, NodeId>, c: Change) -> Effect {
        apply_change(tree, rn, extra, c, 1_700_000_000)
    }

    #[test]
    fn growing_file_updates_every_folder_above_it() {
        let (mut t, rn) = fixture();
        let mut extra = HashMap::new();
        let e = run(
            &mut t,
            &rn,
            &mut extra,
            Change::Resized {
                frn: 17 | SEQ,
                name: "A.TXT".into(),
                size: 5_000,
                alloc: 8192,
            },
        );
        assert_eq!(e, Effect::Resized(rn[17]));
        assert_eq!(t.root_stats().0, 5_050);
        assert_eq!(t.children(0)[0], rn[16], "docs is now the biggest folder entry");
    }

    #[test]
    fn a_reused_record_number_does_not_resize_a_stranger() {
        let (mut t, rn) = fixture();
        let mut extra = HashMap::new();
        let e = run(
            &mut t,
            &rn,
            &mut extra,
            Change::Resized {
                frn: 17,
                name: "other.log".into(),
                size: 9_999,
                alloc: 9_999,
            },
        );
        assert_eq!(e, Effect::Ignored);
        assert_eq!(t.root_stats().0, 150);
    }

    #[test]
    fn created_files_appear_then_grow_then_disappear() {
        let (mut t, rn) = fixture();
        let mut extra = HashMap::new();
        let e = run(
            &mut t,
            &rn,
            &mut extra,
            Change::Created {
                frn: 20 | SEQ,
                parent_frn: 16,
                name: "new.log".into(),
                is_dir: false,
                size: 10,
                alloc: 4096,
            },
        );
        assert_eq!(e, Effect::Structure);
        assert_eq!(t.root_stats(), (160, 3 * 4096, 3));
        let e = run(
            &mut t,
            &rn,
            &mut extra,
            Change::Resized {
                frn: 20 | SEQ,
                name: "new.log".into(),
                size: 1 << 30,
                alloc: 1 << 30,
            },
        );
        assert!(matches!(e, Effect::Resized(_)), "files created live are tracked: {e:?}");
        assert_eq!(t.root_stats().0, 150 + (1 << 30));
        assert_eq!(t.children(0)[0], rn[16]);
        let e = run(
            &mut t,
            &rn,
            &mut extra,
            Change::Deleted {
                frn: 20 | SEQ,
                name: "new.log".into(),
            },
        );
        assert_eq!(e, Effect::Structure);
        assert_eq!(t.root_stats(), (150, 2 * 4096, 2));
    }

    #[test]
    fn a_new_folder_can_receive_files_and_unknown_parents_are_reported() {
        let (mut t, rn) = fixture();
        let mut extra = HashMap::new();
        let dir = Change::Created {
            frn: 21,
            parent_frn: 5,
            name: "cache".into(),
            is_dir: true,
            size: 0,
            alloc: 0,
        };
        assert_eq!(run(&mut t, &rn, &mut extra, dir), Effect::Structure);
        let file = Change::Created {
            frn: 22,
            parent_frn: 21,
            name: "blob".into(),
            is_dir: false,
            size: 700,
            alloc: 4096,
        };
        assert_eq!(run(&mut t, &rn, &mut extra, file), Effect::Structure);
        assert_eq!(t.root_stats().0, 850);
        let orphan = Change::Created {
            frn: 23,
            parent_frn: 30,
            name: "x".into(),
            is_dir: false,
            size: 1,
            alloc: 1,
        };
        assert_eq!(run(&mut t, &rn, &mut extra, orphan), Effect::Unplaced);
    }

    #[test]
    fn replayed_creation_of_a_known_file_is_a_resize_not_a_duplicate() {
        let (mut t, rn) = fixture();
        let mut extra = HashMap::new();
        let len = t.len();
        let c = Change::Created {
            frn: 17,
            parent_frn: 16,
            name: "a.txt".into(),
            is_dir: false,
            size: 120,
            alloc: 4096,
        };
        assert!(matches!(run(&mut t, &rn, &mut extra, c), Effect::Resized(_)));
        assert_eq!(t.len(), len);
        assert_eq!(t.root_stats().0, 170);
    }

    #[test]
    fn renames_within_a_folder_and_moves_of_files() {
        let (mut t, rn) = fixture();
        let mut extra = HashMap::new();
        let r = Change::Renamed {
            frn: 17,
            parent_frn: 16,
            name: "renamed.txt".into(),
            is_dir: false,
            size: 100,
            alloc: 4096,
        };
        assert_eq!(run(&mut t, &rn, &mut extra, r), Effect::Structure);
        assert_eq!(t.name(rn[17]), "renamed.txt");
        // b.bin moves into docs
        let mv = Change::Renamed {
            frn: 18,
            parent_frn: 16,
            name: "b.bin".into(),
            is_dir: false,
            size: 50,
            alloc: 4096,
        };
        assert_eq!(run(&mut t, &rn, &mut extra, mv), Effect::Structure);
        assert!(t.is_removed(rn[18]));
        assert_eq!(t.node(rn[16]).size, 150);
        assert_eq!(t.root_stats().0, 150);
        // renaming a folder where it is works ...
        let ren = Change::Renamed {
            frn: 16,
            parent_frn: 5,
            name: "docs2".into(),
            is_dir: true,
            size: 0,
            alloc: 0,
        };
        assert_eq!(run(&mut t, &rn, &mut extra, ren), Effect::Structure);
        assert_eq!(t.name(rn[16]), "docs2");
        // ... moving it under another folder is not supported and says so
        let other = Change::Created {
            frn: 21,
            parent_frn: 5,
            name: "other".into(),
            is_dir: true,
            size: 0,
            alloc: 0,
        };
        run(&mut t, &rn, &mut extra, other);
        let mv = Change::Renamed {
            frn: 16,
            parent_frn: 21,
            name: "docs2".into(),
            is_dir: true,
            size: 0,
            alloc: 0,
        };
        assert_eq!(run(&mut t, &rn, &mut extra, mv), Effect::Unplaced);
    }

    #[test]
    fn deleting_checks_the_name_and_unknown_records_are_ignored() {
        let (mut t, rn) = fixture();
        let mut extra = HashMap::new();
        assert_eq!(
            run(
                &mut t,
                &rn,
                &mut extra,
                Change::Deleted {
                    frn: 17,
                    name: "someone-else.txt".into()
                }
            ),
            Effect::Ignored
        );
        assert_eq!(
            run(
                &mut t,
                &rn,
                &mut extra,
                Change::Deleted {
                    frn: 31,
                    name: "x".into()
                }
            ),
            Effect::Ignored
        );
        assert_eq!(
            run(
                &mut t,
                &rn,
                &mut extra,
                Change::Deleted {
                    frn: u64::MAX,
                    name: "x".into()
                }
            ),
            Effect::Ignored
        );
        assert_eq!(
            run(
                &mut t,
                &rn,
                &mut extra,
                Change::Deleted {
                    frn: 17,
                    name: "a.txt".into()
                }
            ),
            Effect::Structure
        );
        assert_eq!(t.root_stats().0, 50);
    }
}
