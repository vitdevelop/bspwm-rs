//! Property tests for the invariants `docs/bsp-core.md` promises:
//!
//! - every leaf maps to one window,
//! - split ratios stay in `[0, 1]`,
//! - no window is lost,
//! - rectangles tile the desktop without overlap.
//!
//! Each test runs a random sequence of tree operations and checks the
//! invariant still holds afterward, rather than asserting one fixed
//! before/after shape (that is what the unit tests in `src/tree.rs` are
//! for).

use bsp_core::geometry::Rect;
use bsp_core::id::WindowId;
use bsp_core::node::Client;
use bsp_core::settings::Settings;
use bsp_core::tree::{CirculateDir, FlipAxis, Layout, Tree};
use proptest::prelude::*;

/// A structural operation to apply at the tree's root, for property tests
/// that only care about invariants a well-formed tree must keep no matter
/// what sequence of these ran.
#[derive(Debug, Clone, Copy)]
enum StructuralOp {
    Rotate90,
    Rotate180,
    Rotate270,
    FlipHorizontal,
    FlipVertical,
    Equalize,
    Balance,
    CirculateForward,
    CirculateBackward,
}

fn structural_op() -> impl Strategy<Value = StructuralOp> {
    prop_oneof![
        Just(StructuralOp::Rotate90),
        Just(StructuralOp::Rotate180),
        Just(StructuralOp::Rotate270),
        Just(StructuralOp::FlipHorizontal),
        Just(StructuralOp::FlipVertical),
        Just(StructuralOp::Equalize),
        Just(StructuralOp::Balance),
        Just(StructuralOp::CirculateForward),
        Just(StructuralOp::CirculateBackward),
    ]
}

fn apply_op(t: &mut Tree, s: &Settings, op: StructuralOp) {
    let root = t.root;
    match op {
        StructuralOp::Rotate90 => t.rotate_tree(root, 90),
        StructuralOp::Rotate180 => t.rotate_tree(root, 180),
        StructuralOp::Rotate270 => t.rotate_tree(root, 270),
        StructuralOp::FlipHorizontal => t.flip_tree(root, FlipAxis::Horizontal),
        StructuralOp::FlipVertical => t.flip_tree(root, FlipAxis::Vertical),
        StructuralOp::Equalize => t.equalize_tree(root, s),
        StructuralOp::Balance => {
            t.balance_tree(root);
        }
        StructuralOp::CirculateForward => t.circulate_leaves(s, root, CirculateDir::Forward),
        StructuralOp::CirculateBackward => t.circulate_leaves(s, root, CirculateDir::Backward),
    }
}

/// Builds a tree with `n` client leaves, each inserted next to a
/// previously inserted leaf chosen by `anchor_picks` (values are reduced
/// modulo the current leaf count), and returns the tree plus the window
/// ids in insertion order.
fn build_tree(settings: &Settings, n: usize, anchor_picks: &[usize]) -> (Tree, Vec<u32>) {
    let mut t = Tree::new();
    let mut leaves = Vec::new();
    let mut windows = Vec::new();

    for i in 0..n {
        let window = i as u32 + 1;
        let client = Client::new(WindowId(window), settings.border_width);
        let id = t.new_client_node(settings, client);
        let anchor = if leaves.is_empty() {
            None
        } else {
            let pick = anchor_picks.get(i).copied().unwrap_or(0);
            Some(leaves[pick % leaves.len()])
        };
        t.insert_node(settings, id, anchor);
        leaves.push(id);
        windows.push(window);
    }

    (t, windows)
}

fn leaf_windows(t: &Tree) -> Vec<u32> {
    let mut out = Vec::new();
    let mut f = t.first_extrema(t.root);
    while let Some(n) = f {
        out.push(t.node(n).client.as_ref().unwrap().window.0);
        f = t.next_leaf(Some(n), t.root);
    }
    out
}

fn all_ratios_in_bounds(t: &Tree, id: Option<bsp_core::id::NodeId>) -> bool {
    let Some(id) = id else { return true };
    let node = t.node(id);
    if t.is_leaf(id) {
        return true;
    }
    (0.0..=1.0).contains(&node.split_ratio)
        && all_ratios_in_bounds(t, node.first_child())
        && all_ratios_in_bounds(t, node.second_child())
}

proptest! {
    /// Every window inserted is present as exactly one leaf, no matter
    /// what structural operations ran afterward, and no window not
    /// inserted ever appears.
    #[test]
    fn every_window_maps_to_exactly_one_leaf(
        n in 1usize..8,
        anchor_picks in prop::collection::vec(0usize..8, 0..8),
        ops in prop::collection::vec(structural_op(), 0..12),
    ) {
        let settings = Settings::default();
        let (mut t, mut expected) = build_tree(&settings, n, &anchor_picks);
        expected.sort_unstable();

        for op in ops {
            apply_op(&mut t, &settings, op);
        }

        let mut actual = leaf_windows(&t);
        actual.sort_unstable();
        prop_assert_eq!(actual, expected);
    }

    /// Split ratios never leave `[0, 1]`.
    #[test]
    fn split_ratios_stay_in_bounds(
        n in 2usize..8,
        anchor_picks in prop::collection::vec(0usize..8, 0..8),
        ops in prop::collection::vec(structural_op(), 0..12),
    ) {
        let settings = Settings::default();
        let (mut t, _) = build_tree(&settings, n, &anchor_picks);

        prop_assert!(all_ratios_in_bounds(&t, t.root));
        for op in ops {
            apply_op(&mut t, &settings, op);
            prop_assert!(all_ratios_in_bounds(&t, t.root));
        }
    }

    /// After layout, every leaf's rectangle is one non-overlapping piece
    /// of a strict partition of the desktop's rectangle: the leaf
    /// rectangles' areas sum to exactly the desktop's area (bspwm's BSP
    /// split always divides a rectangle into exactly two, so by
    /// induction the leaves always exactly tile the root, whatever the
    /// tree's shape or split ratios).
    #[test]
    fn layout_tiles_the_desktop_without_gaps_or_overlap(
        n in 1usize..8,
        anchor_picks in prop::collection::vec(0usize..8, 0..8),
        ops in prop::collection::vec(structural_op(), 0..12),
    ) {
        let settings = Settings::default();
        let (mut t, _) = build_tree(&settings, n, &anchor_picks);
        for op in ops {
            apply_op(&mut t, &settings, op);
        }

        let mrect = Rect::new(0, 0, 1920, 1080);
        t.apply_layout(t.root, mrect, 0, Layout::Tiled, mrect, bsp_core::tree::LayoutOptions::default());

        let mut leaves = Vec::new();
        let mut f = t.first_extrema(t.root);
        while let Some(n) = f {
            leaves.push(t.node(n).rect);
            f = t.next_leaf(Some(n), t.root);
        }

        let total_area: i64 = leaves.iter().map(Rect::area).sum();
        prop_assert_eq!(total_area, mrect.area());

        for leaf in &leaves {
            prop_assert!(mrect.contains_rect(leaf));
        }
        for i in 0..leaves.len() {
            for j in (i + 1)..leaves.len() {
                prop_assert!(
                    !rects_overlap(&leaves[i], &leaves[j]),
                    "leaves {} and {} overlap: {:?} vs {:?}",
                    i, j, leaves[i], leaves[j]
                );
            }
        }
    }

    /// Removing every inserted window, in any order, always empties the
    /// tree: no window is ever "lost" in a way that leaves a dangling
    /// half-removed node behind.
    #[test]
    fn removing_every_window_empties_the_tree(
        n in 1usize..8,
        anchor_picks in prop::collection::vec(0usize..8, 0..8),
        removal_order in prop::collection::vec(0usize..8, 1..8),
    ) {
        let settings = Settings::default();
        let (mut t, _) = build_tree(&settings, n, &anchor_picks);

        let mut remaining: Vec<_> = {
            let mut v = Vec::new();
            let mut f = t.first_extrema(t.root);
            while let Some(id) = f {
                v.push(id);
                f = t.next_leaf(Some(id), t.root);
            }
            v
        };

        let mut i = 0;
        while !remaining.is_empty() {
            let pick = removal_order.get(i % removal_order.len().max(1)).copied().unwrap_or(0);
            let idx = pick % remaining.len();
            let id = remaining.remove(idx);
            t.remove_node(&settings, id);
            i += 1;
        }

        prop_assert_eq!(t.root, None);
        prop_assert_eq!(t.focus, None);
    }
}

/// Every parent/child link agrees, and a tree with `n` leaves has `2n - 1` nodes.
fn tree_is_well_formed(t: &Tree) -> bool {
    let leaves = leaf_windows(t).len();
    let ids = t.node_ids();
    let expected = if leaves == 0 { 0 } else { 2 * leaves - 1 };
    if ids.len() != expected {
        return false;
    }
    ids.iter().all(|&id| {
        let node = t.node(id);
        let children_ok = [node.first_child(), node.second_child()]
            .into_iter()
            .flatten()
            .all(|c| t.node(c).parent() == Some(id));
        let parent_ok = match node.parent() {
            Some(p) => t.node(p).first_child() == Some(id) || t.node(p).second_child() == Some(id),
            None => t.root == Some(id),
        };
        children_ok && parent_ok && (node.first_child().is_some() == node.second_child().is_some())
    })
}

proptest! {
    /// Moving, swapping and circulating between two trees never loses or
    /// duplicates a window and never leaves a broken link.
    #[test]
    fn transplants_and_swaps_between_two_trees_keep_every_window(
        na in 1usize..6,
        nb in 1usize..6,
        picks in prop::collection::vec(0usize..8, 0..8),
        ops in prop::collection::vec((0u8..5, 0usize..16, 0usize..16), 0..24),
    ) {
        let settings = Settings::default();
        let (mut a, wa) = build_tree(&settings, na, &picks);
        // Window ids of the second tree do not overlap the first's.
        let mut b = Tree::new();
        let mut wb = Vec::new();
        for i in 0..nb {
            let window = 100 + i as u32;
            let id = b.new_client_node(&settings, Client::new(WindowId(window), settings.border_width));
            let anchor = b.first_extrema(b.root).filter(|_| i > 0);
            b.insert_node(&settings, id, anchor);
            wb.push(window);
        }
        let mut expected: Vec<u32> = wa.iter().chain(wb.iter()).copied().collect();
        expected.sort_unstable();
        let leaf_at = |t: &Tree, pick: usize| {
            let mut leaves = Vec::new();
            let mut f = t.first_extrema(t.root);
            while let Some(n) = f {
                leaves.push(n);
                f = t.next_leaf(Some(n), t.root);
            }
            (!leaves.is_empty()).then(|| leaves[pick % leaves.len()])
        };
        for (kind, x, y) in ops {
            match kind {
                0 => {
                    if let (Some(n1), Some(n2)) = (leaf_at(&a, x), leaf_at(&a, y)) {
                        a.swap_nodes(n1, n2);
                    }
                }
                1 => {
                    // Move a leaf of `a` next to a leaf of `b` (only while `a` keeps one).
                    if leaf_windows(&a).len() > 1 {
                        if let Some(n) = leaf_at(&a, x) {
                            let anchor = leaf_at(&b, y);
                            a.transplant_to_mapped(&settings, n, &mut b, anchor);
                        }
                    }
                }
                2 => {
                    if let (Some(n1), Some(n2)) = (leaf_at(&a, x), leaf_at(&b, y)) {
                        a.swap_subtrees_with(n1, &mut b, n2);
                    }
                }
                3 => {
                    let root = b.root;
                    b.circulate_leaves(&settings, root, CirculateDir::Forward);
                }
                _ => {
                    if leaf_windows(&b).len() > 1 {
                        if let Some(n) = leaf_at(&b, x) {
                            let anchor = leaf_at(&a, y);
                            b.transplant_to_mapped(&settings, n, &mut a, anchor);
                        }
                    }
                }
            }
        }
        let mut actual: Vec<u32> = leaf_windows(&a).into_iter().chain(leaf_windows(&b)).collect();
        actual.sort_unstable();
        prop_assert_eq!(actual, expected);
        prop_assert!(tree_is_well_formed(&a));
        prop_assert!(tree_is_well_formed(&b));
        prop_assert!(all_ratios_in_bounds(&a, a.root));
        prop_assert!(all_ratios_in_bounds(&b, b.root));
    }
}

fn rects_overlap(a: &Rect, b: &Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}
