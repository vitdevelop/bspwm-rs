//! Stable, never-reused node ids for the wire protocol.
//!
//! bspwm assigns every `node_t` an id once, via `xcb_generate_id()`
//! (`src/tree.c` `make_node()`), and never reassigns or reuses it — a
//! `bspc query -n <id>` result stays valid for that node's whole lifetime,
//! including across `bspc node -d`/`-m` (moving it to another desktop or
//! monitor).
//!
//! `bsp-core`'s [`bsp_core::id::NodeId`] cannot serve this role directly:
//! it is an arena index, recycled from a free list once a node is freed
//! (`docs/bsp-core.md`, Core scope), and reassigned to a *different*
//! node entirely once [`bsp_core::tree::Tree::transplant_to`] moves a node
//! into another desktop's tree. This registry sits between the two: it
//! mints a stable `u32` the first time a node is seen, and the executor
//! (`crate::exec`) keeps the mapping pointed at the node's current
//! `(DesktopId, NodeId)` as it moves, so the stable id — the one printed
//! over the wire — never changes for as long as the node exists.

use std::collections::HashMap;

use bsp_core::id::{DesktopId, NodeId};

/// Maps stable, wire-visible node ids to their current `(DesktopId,
/// NodeId)` location, and back.
#[derive(Debug, Clone, Default)]
pub struct NodeRegistry {
    next: u32,
    forward: HashMap<(DesktopId, NodeId), u32>,
    backward: HashMap<u32, (DesktopId, NodeId)>,
}

impl NodeRegistry {
    /// Creates an empty registry. Ids start at 1 (0 is reserved as "none",
    /// as in bspwm's use of `XCB_NONE`, matching `bsp_core::id::IdGen`).
    pub fn new() -> Self {
        Self {
            next: 0,
            forward: HashMap::new(),
            backward: HashMap::new(),
        }
    }

    /// Registers a newly created node, minting a fresh stable id for it.
    /// Call once per node, when the executor creates it (an existing
    /// mapping for `(desktop, node)` is unexpected and replaced).
    pub fn register(&mut self, desktop: DesktopId, node: NodeId) -> u32 {
        self.next += 1;
        let id = self.next;
        self.forward.insert((desktop, node), id);
        self.backward.insert(id, (desktop, node));
        id
    }

    /// Forgets a freed node's mapping.
    pub fn unregister(&mut self, desktop: DesktopId, node: NodeId) {
        if let Some(id) = self.forward.remove(&(desktop, node)) {
            self.backward.remove(&id);
        }
    }

    /// Re-points an existing node's mapping to its new location, keeping
    /// its stable id unchanged. Call after
    /// [`bsp_core::tree::Tree::transplant_to`] mints a new arena `NodeId`
    /// in the destination tree for what is, from the wire protocol's point
    /// of view, the same node.
    pub fn relocate(&mut self, old: (DesktopId, NodeId), new: (DesktopId, NodeId)) {
        if let Some(id) = self.forward.remove(&old) {
            self.forward.insert(new, id);
            self.backward.insert(id, new);
        }
    }

    /// The stable id for a node currently at `(desktop, node)`, if it is
    /// registered.
    pub fn id_of(&self, desktop: DesktopId, node: NodeId) -> Option<u32> {
        self.forward.get(&(desktop, node)).copied()
    }

    /// The current location of the node with stable id `id`, if it is
    /// registered.
    pub fn lookup(&self, id: u32) -> Option<(DesktopId, NodeId)> {
        self.backward.get(&id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsp_core::settings::Settings;
    use bsp_core::tree::Tree;

    /// Two arena `NodeId`s from independent, freshly made trees — enough
    /// to exercise the registry without needing real window/tree state.
    fn two_node_ids() -> (NodeId, NodeId) {
        let settings = Settings::default();
        let mut t1 = Tree::new();
        let mut t2 = Tree::new();
        (t1.new_node(&settings), t2.new_node(&settings))
    }

    #[test]
    fn register_mints_increasing_ids_starting_at_one() {
        let mut reg = NodeRegistry::new();
        let (n1, n2) = two_node_ids();
        let d = DesktopId(1);
        let a = reg.register(d, n1);
        let b = reg.register(d, n2);
        assert_eq!(a, 1);
        assert_eq!(b, 2);
    }

    #[test]
    fn lookup_and_id_of_round_trip() {
        let mut reg = NodeRegistry::new();
        let (n, _) = two_node_ids();
        let d = DesktopId(1);
        let id = reg.register(d, n);
        assert_eq!(reg.lookup(id), Some((d, n)));
        assert_eq!(reg.id_of(d, n), Some(id));
    }

    #[test]
    fn unregister_forgets_the_mapping() {
        let mut reg = NodeRegistry::new();
        let (n, _) = two_node_ids();
        let d = DesktopId(1);
        let id = reg.register(d, n);
        reg.unregister(d, n);
        assert_eq!(reg.lookup(id), None);
        assert_eq!(reg.id_of(d, n), None);
    }

    #[test]
    fn relocate_keeps_the_stable_id_but_moves_the_target() {
        let mut reg = NodeRegistry::new();
        let (n1, n2) = two_node_ids();
        let d1 = DesktopId(1);
        let d2 = DesktopId(2);
        let id = reg.register(d1, n1);
        reg.relocate((d1, n1), (d2, n2));
        assert_eq!(reg.lookup(id), Some((d2, n2)));
        assert_eq!(reg.id_of(d1, n1), None);
        assert_eq!(reg.id_of(d2, n2), Some(id));
    }
}
