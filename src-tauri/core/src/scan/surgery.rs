//! Tree surgery (spec §9 — the build spec's after-recycling step): remove nodes
//! from the in-memory tree WITHOUT a rescan.
//!
//! Steps (per spec): mark nodes `removed`, subtract sizes, counts and
//! `type_sizes` from every ancestor, compact and re-sort the affected
//! `order` slices. Everything here is pure data manipulation over
//! `&mut Tree` — host-testable, no Windows APIs (D10).
//!
//! Ids are NEVER reused: the arena only grows, surgery flags nodes.
//! `children_sorted` consumers filter `is_removed()` (the command layer
//! already does), so removed ids in the arena are inert.
//!
//! Known limitation (documented, not hidden): `max_descendant_modified`
//! cannot be cheaply un-maxed after removal; it stays at the subtree's
//! latest mtime. The spec's surgery list covers sizes/counts/`type_sizes`
//! only; the stale-but-conservative mtime is displayed as-is.

use crate::scan::node::Tree;

/// What one surgery call removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemovedSummary {
    /// Number of nodes marked removed (subtree totals).
    pub nodes: u64,
    /// On-disk bytes subtracted from the ancestors.
    pub on_disk: u64,
    /// Logical bytes subtracted.
    pub logical: u64,
    /// Descendant files subtracted.
    pub files: u64,
    /// Descendant folders subtracted (including the removed folder roots).
    pub folders: u64,
}

/// Mark `roots` (and their subtrees) removed and subtract their
/// aggregates from every surviving ancestor. Roots nested inside
/// another root are absorbed by it (no double subtraction). The CSR
/// `order` is compacted: every folder's slice drops removed ids — the
/// surviving siblings keep their size-sorted order (their own on-disk
/// sizes did not change). `generation` bumps by one.
///
/// The scan root itself is never removable: passing it (or the tree's
/// root id) is a no-op returning an empty summary. The contract used to
/// be enforced only by one caller's `id > 0` filter; it now holds at the
/// API boundary.
///
/// # Panics
/// Never in release: arena ids are validated by the caller (the command
/// layer); a bad id is ignored defensively here.
pub fn remove_subtrees(tree: &mut Tree, roots: &[u32]) -> RemovedSummary {
    // Dedup: keep only roots not nested inside another root.
    let mut effective: Vec<u32> = Vec::with_capacity(roots.len());
    for &r in roots {
        if r == tree.root || tree.node(r).is_none() {
            continue; // Defensive: caller validates; the root always survives.
        }
        let nested = effective.iter().any(|&e| tree.is_descendant_of(r, e));
        if !nested {
            effective.retain(|&e| !tree.is_descendant_of(e, r));
            effective.push(r);
        }
    }

    let mut summary = RemovedSummary::default();
    for &root in &effective {
        remove_one(tree, root, &mut summary);
    }
    if summary.nodes > 0 {
        compact_order(tree);
        tree.generation = tree.generation.saturating_add(1);
    }
    summary
}

/// Remove one subtree: mark + subtract from the ancestor chain.
fn remove_one(tree: &mut Tree, root: u32, summary: &mut RemovedSummary) {
    let Some(root_node) = tree.node(root) else {
        return;
    };
    if root_node.is_removed() {
        return; // Already gone (double-commit is a no-op, not an error).
    }
    // Subtree totals (root aggregate = subtree sum by rollup invariant).
    let sub_on_disk = root_node.on_disk;
    let sub_logical = root_node.logical;
    let (sub_files, sub_folders, sub_types) = if root_node.is_dir() {
        match tree.dir_extras.get(root_node.dir_index as usize) {
            Some(e) => (e.file_count, e.folder_count, e.type_sizes),
            None => (0, 0, [0; 9]),
        }
    } else {
        // A file root subtracts exactly one file (its own) — `file_count`
        // is a *descendant* count, so the file itself is not in the
        // parent's extras yet roll-up counted it (`pe.file_count += 1`).
        let mut t = [0u64; 9];
        t[(root_node.category().as_bits() as usize).min(8)] = root_node.on_disk;
        (1, 0, t)
    };
    // The folder root itself counts as one folder removed from its
    // parent's folder_count (DirExtra counts descendants only).
    let folders_removed = if root_node.is_dir() {
        sub_folders.saturating_add(1)
    } else {
        0
    };

    // Mark the whole subtree removed (iterative stack, arena order).
    let mut stack: Vec<u32> = vec![root];
    let mut marked: u64 = 0;
    while let Some(id) = stack.pop() {
        let Some(n) = tree.arena.get_mut(id as usize) else {
            continue;
        };
        if n.is_removed() {
            continue;
        }
        n.set_removed(true);
        marked = marked.saturating_add(1);
        if n.child_count > 0 && n.dir_index != u32::MAX {
            let fc = n.first_child;
            let cc = n.child_count;
            stack.extend((fc..fc + cc).rev());
        }
    }

    // Subtract from every surviving ancestor of root.
    let mut ancestor = tree.arena[root as usize].parent;
    while ancestor != u32::MAX {
        let a = &mut tree.arena[ancestor as usize];
        if a.is_removed() {
            break; // A removed ancestor was already subtracted.
        }
        a.on_disk = a.on_disk.saturating_sub(sub_on_disk);
        a.logical = a.logical.saturating_sub(sub_logical);
        if a.is_dir() && a.dir_index != u32::MAX {
            if let Some(e) = tree.dir_extras.get_mut(a.dir_index as usize) {
                e.file_count = e.file_count.saturating_sub(sub_files);
                e.folder_count = e.folder_count.saturating_sub(folders_removed);
                for (i, &t) in sub_types.iter().enumerate() {
                    e.type_sizes[i] = e.type_sizes[i].saturating_sub(t);
                }
            }
        }
        ancestor = tree.arena[ancestor as usize].parent;
    }

    summary.nodes = summary.nodes.saturating_add(marked);
    summary.on_disk = summary.on_disk.saturating_add(sub_on_disk);
    summary.logical = summary.logical.saturating_add(sub_logical);
    summary.files = summary.files.saturating_add(sub_files);
    summary.folders = summary.folders.saturating_add(folders_removed);
}

/// Rebuild the CSR `order`: drop removed ids from every folder slice.
/// Sibling order stays valid (surviving children kept their own sizes).
///
/// Iterates the ARENA and reaches each folder's extras through its
/// `dir_index` — `dir_extras` indices are NOT arena ids (they count
/// directories only), so the two index spaces must never be conflated
/// (the historical bug this comment guards against).
fn compact_order(tree: &mut Tree) {
    let mut new_order: Vec<u32> = Vec::with_capacity(tree.order.len());
    let mut extras = std::mem::take(&mut tree.dir_extras);
    for n in &tree.arena {
        if !n.is_dir() || n.dir_index == u32::MAX {
            continue; // Files have no extras slot.
        }
        let extra = &mut extras[n.dir_index as usize];
        if n.is_removed() {
            // Removed folders keep their slice bookkeeping inert; the
            // arena-level removed flag already filters them everywhere.
            extra.order_count = 0;
            extra.order_offset = 0;
            continue;
        }
        let old = &tree.order
            [extra.order_offset as usize..(extra.order_offset + extra.order_count) as usize];
        extra.order_offset = new_order.len() as u32;
        // Mirrors `rollup::build_order`: keep every non-removed child,
        // including zero-`on_disk` ones (MFT-resident tiny files) — a
        // filter mismatch here made zero-size children silently vanish
        // from every folder after the first cleanup.
        let kept: Vec<u32> = old
            .iter()
            .copied()
            .filter(|&id| {
                tree.arena
                    .get(id as usize)
                    .map_or(true, |c| !c.is_removed())
            })
            .collect();
        extra.order_count = kept.len() as u32;
        new_order.extend(kept);
    }
    tree.dir_extras = extras;
    tree.order = new_order;
}

/// Fix the UI navigation point after surgery: when the current folder
/// was removed, walk UP to the first surviving ancestor (the scan root
/// always survives — it is never removable by this API's contract).
///
/// # Panics
/// Never: a missing id maps to the root.
#[must_use]
pub fn fixup_navigation(tree: &Tree, current: u32) -> u32 {
    let mut id = current;
    loop {
        if let Some(n) = tree.node(id) {
            if !n.is_removed() {
                return id;
            }
        }
        let parent = tree
            .node(id)
            .map_or(0, |n| if n.parent == u32::MAX { id } else { n.parent });
        if parent == id {
            return 0; // Root reached (never removed).
        }
        id = parent;
    }
}

/// True when a node (or an ancestor) was removed — selection fixup.
#[must_use]
pub fn node_gone(tree: &Tree, id: u32) -> bool {
    tree.node(id).map_or(true, super::node::Node::is_removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::node::{BatchEntry, Node, Tree};
    use crate::scan::rollup;

    fn dir(name: &str) -> BatchEntry {
        let mut node = Node::new_dir();
        node.modified = 1;
        BatchEntry {
            name: name.encode_utf16().collect(),
            node,
        }
    }

    fn file(name: &str, size: u64) -> BatchEntry {
        let mut node = Node::new_file();
        node.logical = size;
        node.on_disk = size;
        node.modified = 2;
        node.set_category(crate::scan::categories::FileCategory::from_name(
            &name.encode_utf16().collect::<Vec<u16>>(),
        ));
        BatchEntry {
            name: name.encode_utf16().collect(),
            node,
        }
    }

    fn build() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\S");
        t.append_batch(0, vec![dir("a"), dir("b"), file("root.bin", 10)]);
        t.append_batch(
            1,
            vec![file("a1.mp4", 100), file("a2.txt", 20), dir("deep")],
        );
        t.append_batch(6, vec![file("d1.mp4", 40)]);
        t.append_batch(2, vec![file("b1.bin", 5)]);
        rollup::finalize(&mut t);
        t
    }

    #[test]
    fn removes_subtree_and_subtracts_ancestors() {
        let mut t = build();
        // ids: 0 root, 1 a, 2 b, 3 root.bin, 4 a1, 5 a2, 6 deep, 7 d1, 8 b1.
        let s = remove_subtrees(&mut t, &[1]);
        // a, a1, a2, deep, d1 — the whole subtree.
        assert_eq!(s.nodes, 5);
        assert_eq!(s.on_disk, 160);
        assert_eq!(s.files, 3);
        assert_eq!(s.folders, 2); // a + deep
                                  // Root totals: 175 → 15.
        let root = t.node(0).unwrap();
        assert_eq!(root.on_disk, 15);
        let root_extra = &t.dir_extras[root.dir_index as usize];
        assert_eq!(root_extra.file_count, 2); // root.bin + b1
        assert_eq!(root_extra.folder_count, 1); // b
                                                // Video type bytes: 140 → 0.
        assert_eq!(
            root_extra.type_sizes[crate::scan::categories::FileCategory::Video.as_bits() as usize],
            0
        );
        // Generation bumped.
        assert_eq!(t.generation, 2);
        // children_sorted no longer lists a.
        let kids = t.children_sorted(0).to_vec();
        assert!(!kids.contains(&1));
        assert!(kids.contains(&2));
    }

    #[test]
    fn nested_roots_absorbed_no_double_subtract() {
        let mut t = build();
        let s = remove_subtrees(&mut t, &[1, 6]); // deep nested inside a
        assert_eq!(s.on_disk, 160); // not 200
        let root = t.node(0).unwrap();
        assert_eq!(root.on_disk, 15);
    }

    #[test]
    fn double_commit_is_noop() {
        let mut t = build();
        let first = remove_subtrees(&mut t, &[1]);
        let again = remove_subtrees(&mut t, &[1]);
        assert_eq!(first.on_disk, 160);
        assert_eq!(again.on_disk, 0);
        assert_eq!(again.nodes, 0);
        // Generation did NOT bump on the no-op.
        assert_eq!(t.generation, 2);
    }

    #[test]
    fn order_rebuilt_sorted_for_survivors() {
        let mut t = build();
        remove_subtrees(&mut t, &[6]); // remove deep only
        let kids = t.children_sorted(1).to_vec(); // a's remaining children
        assert_eq!(kids, vec![4, 5]); // a1 (100) then a2 (20)
                                      // Compact: 8 entries → 6 (deep's slice emptied, root 3 + a 2 + b 1).
        assert_eq!(t.order.len(), 6);
    }

    #[test]
    fn navigation_fixup_walks_up() {
        let mut t = build();
        remove_subtrees(&mut t, &[6]); // deep removed
        assert_eq!(fixup_navigation(&t, 7), 1); // d1 → a survives
        remove_subtrees(&mut t, &[1]); // a removed too
        assert_eq!(fixup_navigation(&t, 4), 0); // a1 → root
        assert_eq!(fixup_navigation(&t, 0), 0); // root stays
    }

    #[test]
    fn gone_detection() {
        let mut t = build();
        remove_subtrees(&mut t, &[2]);
        assert!(node_gone(&t, 2));
        assert!(node_gone(&t, 8)); // b1 inside b
        assert!(!node_gone(&t, 1));
    }

    /// Regression tree shape for the three 2026-09 surgery bugs: FILES
    /// PRECEDE DIRECTORIES in every batch (real NTFS order — e.g.
    /// `pagefile.sys` sorts before the folders). The original tests all
    /// appended dirs first, which made `dir_index == arena id` hold by
    /// accident and hid the `compact_order` index conflation.
    fn build_files_first() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\S");
        // Batch: file, then dirs — dir_index != arena id from here on.
        t.append_batch(
            0,
            vec![file("pagefile.sys", 30), dir("Program Files"), dir("Users")],
        );
        // ids: 0 root, 1 pagefile, 2 PF (di=1), 3 Users (di=2).
        t.append_batch(2, vec![file("x.dll", 40)]); // 4
        t.append_batch(3, vec![file("y.txt", 100)]); // 5
        rollup::finalize(&mut t);
        t
    }

    /// BUG A (`arena`/`dir_extras` index conflation): removing an UNRELATED
    /// file used to wipe `children_sorted` of every folder whose
    /// `dir_index` collided with a file's arena id — Program Files
    /// (di=1) lost x.dll because `arena[1]` is pagefile.sys, a file.
    #[test]
    fn files_first_tree_survives_unrelated_file_removal() {
        let mut t = build_files_first();
        let before_pf: Vec<u32> = t.children_sorted(2).to_vec();
        let before_users: Vec<u32> = t.children_sorted(3).to_vec();
        assert_eq!(before_pf, vec![4]);
        assert_eq!(before_users, vec![5]);
        let s = remove_subtrees(&mut t, &[5]); // remove Users/y.txt (unrelated to PF)
        assert_eq!(s.nodes, 1);
        // Program Files must still show x.dll.
        assert_eq!(t.children_sorted(2).to_vec(), vec![4]);
        // Users' slice is now empty but VALID (not corrupted).
        assert!(t.children_sorted(3).is_empty());
        // Every folder's slice stays consistent after ANY surgery.
        for id in 0..t.arena.len() as u32 {
            if let Some(n) = t.node(id) {
                if n.is_dir() && !n.is_removed() {
                    for &kid in t.children_sorted(id) {
                        let k = t.node(kid).expect("child in range");
                        assert!(!k.is_removed(), "live folder {id} lists removed kid {kid}");
                    }
                }
            }
        }
    }

    /// BUG A companion: the same conflation also zeroed `order` slices of
    /// folders whose extras slot was never reached at all.
    #[test]
    fn files_first_all_folders_keep_slices_after_dir_removal() {
        let mut t = build_files_first();
        remove_subtrees(&mut t, &[3]); // remove Users (dir)
        assert_eq!(t.children_sorted(2).to_vec(), vec![4], "PF keeps x.dll");
        assert_eq!(
            t.children_sorted(0).to_vec(),
            vec![2, 1],
            "root keeps PF + pagefile, size-desc"
        );
    }

    /// BUG B: removing a FILE must decrement every ancestor's
    /// `file_count` (roll-up counted it; surgery forgot to subtract).
    #[test]
    fn file_removal_decrements_ancestor_file_count() {
        let mut t = build_files_first();
        let root_extra = &t.dir_extras[t.node(0).unwrap().dir_index as usize];
        assert_eq!(root_extra.file_count, 3);
        let s = remove_subtrees(&mut t, &[5]); // y.txt
        assert_eq!(s.files, 1, "summary counts the removed file");
        assert_eq!(s.nodes, 1);
        let root_extra = &t.dir_extras[t.node(0).unwrap().dir_index as usize];
        assert_eq!(root_extra.file_count, 2, "root file_count 3 -> 2");
        let users_extra = &t.dir_extras[t.node(3).unwrap().dir_index as usize];
        assert_eq!(users_extra.file_count, 0, "Users file_count 1 -> 0");
    }

    /// BUG D: zero-`on_disk` children (MFT-resident tiny files) survive
    /// `rollup::build_order`; `compact_order` must keep the same policy.
    #[test]
    fn zero_on_disk_child_survives_surgery() {
        // One batch per parent (tree invariant): x.dll + tiny.dat together.
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\S");
        t.append_batch(
            0,
            vec![file("pagefile.sys", 30), dir("Program Files"), dir("Users")],
        );
        // ids: 0 root, 1 pagefile, 2 PF (di=1), 3 Users (di=2).
        let mut tiny = Node::new_file();
        tiny.logical = 12;
        tiny.on_disk = 0; // MFT-resident: on-disk allocation reports 0.
        tiny.set_category(crate::scan::categories::FileCategory::from_name(
            &"tiny.dat".encode_utf16().collect::<Vec<u16>>(),
        ));
        t.append_batch(
            2,
            vec![
                file("x.dll", 40),
                BatchEntry {
                    name: "tiny.dat".encode_utf16().collect(),
                    node: tiny,
                },
            ],
        );
        // id 4 = x.dll, 5 = tiny.dat, 6 = y.txt
        t.append_batch(3, vec![file("y.txt", 100)]);
        rollup::finalize(&mut t);
        let before: Vec<u32> = t.children_sorted(2).to_vec();
        assert_eq!(before.len(), 2, "PF has x.dll + tiny.dat after rollup");
        remove_subtrees(&mut t, &[6]); // unrelated removal in Users
        let after = t.children_sorted(2).to_vec();
        assert_eq!(after.len(), 2, "zero-on_disk tiny.dat must NOT be dropped");
        assert!(after.contains(&5), "tiny.dat (id 5) still listed");
    }

    /// API contract: the scan root is never removable.
    #[test]
    fn root_removal_is_rejected() {
        let mut t = build();
        let s = remove_subtrees(&mut t, &[0]);
        assert_eq!(s.nodes, 0);
        assert_eq!(s.on_disk, 0);
        assert_eq!(t.generation, 1, "no-op must not bump generation");
        assert!(!t.node(0).unwrap().is_removed());
        // And the whole family is still there.
        assert_eq!(t.children_sorted(0).len(), 3);
    }
}
