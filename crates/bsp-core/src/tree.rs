//! The binary space partitioning tree: one arena of nodes per desktop, and
//! every structural operation bspwm performs on it.
//!
//! bspwm: `src/tree.c` and `src/tree.h`. Every public function below names
//! the bspwm function it mirrors. Functions that only exist in bspwm to
//! drive X11 side effects (drawing borders, EWMH, the input focus, the
//! stacking list, `subscribe` reports) are left out: those are
//! `bsp-compositor`'s job once an adapter exists to receive the
//! [`crate::desktop::Desktop`]-level effects this crate will grow in a
//! later step. What remains here is the part bspwm itself calls "the
//! tree": parent/child links, split ratios, vacancy, and the rectangles
//! that come out of them.
//!
//! Memory: nodes live in one arena (`Vec<Option<Node>>`) per [`Tree`],
//! addressed by [`NodeId`], with freed slots recycled from a free list —
//! no `Rc<RefCell>`, matching the Performance budget in `docs/design.md`.

use crate::geometry::Rect;
use crate::id::NodeId;
use crate::node::{Client, ClientState, Layer};
use crate::settings::{AutomaticScheme, Settings};

/// Minimum width a leaf is allowed to shrink to.
///
/// bspwm: `src/tree.h` `MIN_WIDTH`.
pub const MIN_WIDTH: i32 = 32;
/// Minimum height a leaf is allowed to shrink to.
///
/// bspwm: `src/tree.h` `MIN_HEIGHT`.
pub const MIN_HEIGHT: i32 = 32;

/// The orientation of a split.
///
/// bspwm: `src/types.h` `split_type_t`. Naming follows bspwm: `Vertical`
/// divides the rectangle with a vertical line (children side by side, the
/// fence measured along `width`); `Horizontal` divides it with a
/// horizontal line (children stacked, the fence measured along `height`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitType {
    /// Children stacked top/bottom; the fence is measured along height.
    Horizontal,
    /// Children side by side; the fence is measured along width.
    Vertical,
}

/// Which child slot a newly inserted node takes.
///
/// bspwm: `src/types.h` `child_polarity_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildPolarity {
    /// The new node becomes `first_child`.
    First,
    /// The new node becomes `second_child`.
    Second,
}

/// A compass direction, used for preselection and split placement.
///
/// bspwm: `src/types.h` `direction_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Up.
    North,
    /// Left.
    West,
    /// Down.
    South,
    /// Right.
    East,
}

/// The axis `flip_tree` mirrors across.
///
/// bspwm: `src/types.h` `flip_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipAxis {
    /// Mirror left-right splits.
    Horizontal,
    /// Mirror top-bottom splits.
    Vertical,
}

/// Direction `circulate_leaves` rotates tiled leaves in.
///
/// bspwm: `src/types.h` `circulate_dir_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CirculateDir {
    /// Toward the end of leaf order.
    Forward,
    /// Toward the start of leaf order.
    Backward,
}

/// A desktop's layout mode.
///
/// bspwm: `src/types.h` `layout_t`. Lives here rather than in
/// `crate::desktop` because [`apply_layout`](Tree::apply_layout) (bspwm:
/// `src/tree.c`, also declared in `tree.h` rather than `desktop.h`) is the
/// only thing that reads it; `crate::desktop` re-exports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Every leaf gets its computed slot in the split tree.
    Tiled,
    /// Every tiled leaf covers the whole desktop; only the focused one
    /// shows.
    Monocle,
}

/// A pending split, waiting for the next inserted node to consume it.
///
/// bspwm: `src/types.h` `presel_t` (`feedback`, an X window id, is left
/// out: drawing the preselection rectangle is `bsp-compositor`'s job).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Presel {
    /// Ratio the eventual split will use.
    pub split_ratio: f64,
    /// Side of the node the new node will be inserted on.
    pub split_dir: Direction,
}

/// The minimum size a node's subtree is allowed to shrink its siblings to.
///
/// bspwm: `src/types.h` `constraints_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Constraints {
    /// Minimum width.
    pub min_width: i32,
    /// Minimum height.
    pub min_height: i32,
}

impl Default for Constraints {
    fn default() -> Self {
        Self {
            min_width: MIN_WIDTH,
            min_height: MIN_HEIGHT,
        }
    }
}

/// One node in the tree: either an internal split node (`client` is
/// `None`) or a leaf, which is either a receptacle (`client` is `None`,
/// `first_child`/`second_child` are `None`) or a window (`client` is
/// `Some`).
///
/// bspwm: `src/types.h` `node_t`.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// This node's split orientation (meaningless on a leaf).
    pub split_type: SplitType,
    /// This node's split ratio (meaningless on a leaf).
    pub split_ratio: f64,
    /// A pending preselection, if one was set on this node.
    pub presel: Option<Presel>,
    /// The rectangle the layout last computed for this node.
    pub rect: Rect,
    /// The minimum size this node's subtree can shrink to.
    pub constraints: Constraints,
    /// `true` if this node and every descendant hold no client (an empty
    /// receptacle, or the "hole" a floating/fullscreen/hidden client's
    /// former tiled slot leaves behind).
    pub vacant: bool,
    /// `true` if this node is hidden (`bspc node -g hidden`).
    pub hidden: bool,
    /// `true` if this node stays on its monitor's active desktop no
    /// matter which desktop is focused.
    pub sticky: bool,
    /// `true` if automatic insertion must not target this node.
    pub private: bool,
    /// `true` if this node's size and position cannot be changed.
    pub locked: bool,
    /// `true` if this node is marked (for `bspc node <sel> <sel> -s`
    /// style two-argument commands).
    pub marked: bool,
    first_child: Option<NodeId>,
    second_child: Option<NodeId>,
    parent: Option<NodeId>,
    /// The window this leaf shows, if any.
    pub client: Option<Client>,
}

impl Node {
    fn new(settings: &Settings) -> Self {
        Self {
            split_type: SplitType::Vertical,
            split_ratio: settings.split_ratio,
            presel: None,
            rect: Rect::default(),
            constraints: Constraints::default(),
            vacant: false,
            hidden: false,
            sticky: false,
            private: false,
            locked: false,
            marked: false,
            first_child: None,
            second_child: None,
            parent: None,
            client: None,
        }
    }

    /// `true` if this node has no children.
    ///
    /// bspwm: `src/tree.c` `is_leaf()`.
    pub fn is_leaf(&self) -> bool {
        self.first_child.is_none() && self.second_child.is_none()
    }

    /// `true` if this leaf holds no client.
    ///
    /// bspwm: `src/helpers.h` `IS_RECEPTACLE`.
    pub fn is_receptacle(&self) -> bool {
        self.is_leaf() && self.client.is_none()
    }

    /// This node's parent, if any.
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// This node's first child, if any.
    pub fn first_child(&self) -> Option<NodeId> {
        self.first_child
    }

    /// This node's second child, if any.
    pub fn second_child(&self) -> Option<NodeId> {
        self.second_child
    }
}

/// One desktop's tree: an arena of nodes plus the root and focused leaf.
///
/// bspwm keeps `root` and `focus` on `desktop_t`; they live here instead
/// because every operation that touches them also walks the arena, and
/// keeping the three together avoids passing them as three separate
/// parameters everywhere (`crate::desktop::Desktop` embeds a `Tree`).
#[derive(Debug, Clone, Default)]
pub struct Tree {
    nodes: Vec<Option<Node>>,
    free: Vec<u32>,
    /// The tree's root node, or `None` if the desktop is empty.
    pub root: Option<NodeId>,
    /// The focused leaf, or `None` if the desktop is empty or every leaf
    /// is unfocusable (hidden or a receptacle).
    pub focus: Option<NodeId>,
}

impl Tree {
    /// Creates an empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    fn alloc(&mut self, node: Node) -> NodeId {
        if let Some(index) = self.free.pop() {
            self.nodes[index as usize] = Some(node);
            NodeId(index)
        } else {
            let index = self.nodes.len() as u32;
            self.nodes.push(Some(node));
            NodeId(index)
        }
    }

    /// Creates a bare, parentless node (an empty receptacle once inserted).
    ///
    /// bspwm: `src/tree.c` `make_node()`.
    pub fn new_node(&mut self, settings: &Settings) -> NodeId {
        self.alloc(Node::new(settings))
    }

    /// Creates a leaf node already holding `client`.
    pub fn new_client_node(&mut self, settings: &Settings, client: Client) -> NodeId {
        let mut node = Node::new(settings);
        node.client = Some(client);
        self.alloc(node)
    }

    /// Borrows a node. Panics if `id` does not name a live node in this
    /// tree: every `NodeId` in circulation was handed out by this tree and
    /// is invalidated only by [`Tree::remove_node`] or
    /// [`Tree::transplant_to`], so a panic here means a caller kept a
    /// `NodeId` past one of those calls, a programming error to fix rather
    /// than recover from.
    pub fn node(&self, id: NodeId) -> &Node {
        self.nodes[id.index()].as_ref().expect("stale NodeId")
    }

    /// Mutably borrows a node. See [`Tree::node`] on the panic condition.
    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.nodes[id.index()].as_mut().expect("stale NodeId")
    }

    /// `true` if `id` still names a live node in this tree.
    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes
            .get(id.index())
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    // ---- Structural queries ---------------------------------------------

    /// bspwm: `src/tree.c` `is_leaf()`.
    pub fn is_leaf(&self, id: NodeId) -> bool {
        self.node(id).is_leaf()
    }

    /// bspwm: `src/helpers.h` `IS_RECEPTACLE`.
    pub fn is_receptacle(&self, id: NodeId) -> bool {
        self.node(id).is_receptacle()
    }

    /// bspwm: `src/tree.c` `is_first_child()`.
    pub fn is_first_child(&self, id: NodeId) -> bool {
        match self.node(id).parent {
            Some(p) => self.node(p).first_child == Some(id),
            None => false,
        }
    }

    /// bspwm: `src/tree.c` `is_second_child()`.
    pub fn is_second_child(&self, id: NodeId) -> bool {
        match self.node(id).parent {
            Some(p) => self.node(p).second_child == Some(id),
            None => false,
        }
    }

    /// bspwm: `src/tree.c` `brother_tree()`.
    pub fn brother(&self, id: NodeId) -> Option<NodeId> {
        let p = self.node(id).parent?;
        let parent = self.node(p);
        if parent.first_child == Some(id) {
            parent.second_child
        } else {
            parent.first_child
        }
    }

    /// bspwm: `src/tree.c` `is_child()`: `true` if `a` is a child of `b`.
    pub fn is_child(&self, a: Option<NodeId>, b: Option<NodeId>) -> bool {
        match (a, b) {
            (Some(a), Some(_)) => self.node(a).parent == b,
            _ => false,
        }
    }

    /// bspwm: `src/tree.c` `is_descendant()`: `true` if `a` is `b`, or a
    /// descendant of `b`.
    pub fn is_descendant(&self, a: Option<NodeId>, b: Option<NodeId>) -> bool {
        let Some(b) = b else { return false };
        let mut cur = a;
        loop {
            match cur {
                Some(c) if c == b => return true,
                Some(c) => cur = self.node(c).parent,
                None => return false,
            }
        }
    }

    /// bspwm: `src/tree.c` `first_extrema()`.
    pub fn first_extrema(&self, id: Option<NodeId>) -> Option<NodeId> {
        let mut cur = id?;
        while let Some(c) = self.node(cur).first_child {
            cur = c;
        }
        Some(cur)
    }

    /// bspwm: `src/tree.c` `second_extrema()`.
    pub fn second_extrema(&self, id: Option<NodeId>) -> Option<NodeId> {
        let mut cur = id?;
        while let Some(c) = self.node(cur).second_child {
            cur = c;
        }
        Some(cur)
    }

    /// bspwm: `src/tree.c` `next_leaf()`: the next leaf after `id` in
    /// depth-first order, within the subtree rooted at `r`.
    pub fn next_leaf(&self, id: Option<NodeId>, r: Option<NodeId>) -> Option<NodeId> {
        let mut p = id?;
        while self.is_second_child(p) && Some(p) != r {
            p = self.node(p).parent?;
        }
        if Some(p) == r {
            return None;
        }
        let parent = self.node(p).parent?;
        self.first_extrema(self.node(parent).second_child)
    }

    /// bspwm: `src/tree.c` `prev_leaf()`.
    pub fn prev_leaf(&self, id: Option<NodeId>, r: Option<NodeId>) -> Option<NodeId> {
        let mut p = id?;
        while self.is_first_child(p) && Some(p) != r {
            p = self.node(p).parent?;
        }
        if Some(p) == r {
            return None;
        }
        let parent = self.node(p).parent?;
        self.second_extrema(self.node(parent).first_child)
    }

    /// bspwm: `src/tree.c` `next_tiled_leaf()`.
    pub fn next_tiled_leaf(&self, id: Option<NodeId>, r: Option<NodeId>) -> Option<NodeId> {
        let mut next = self.next_leaf(id, r);
        while let Some(n) = next {
            let node = self.node(n);
            if node.client.is_some() && !node.vacant {
                break;
            }
            next = self.next_leaf(Some(n), r);
        }
        next
    }

    /// bspwm: `src/tree.c` `prev_tiled_leaf()`.
    pub fn prev_tiled_leaf(&self, id: Option<NodeId>, r: Option<NodeId>) -> Option<NodeId> {
        let mut prev = self.prev_leaf(id, r);
        while let Some(n) = prev {
            let node = self.node(n);
            if node.client.is_some() && !node.vacant {
                break;
            }
            prev = self.prev_leaf(Some(n), r);
        }
        prev
    }

    /// bspwm: `src/tree.c` `is_focusable()`.
    pub fn is_focusable(&self, id: NodeId) -> bool {
        let mut f = self.first_extrema(Some(id));
        while let Some(n) = f {
            let node = self.node(n);
            if node.client.is_some() && !node.hidden {
                return true;
            }
            f = self.next_leaf(Some(n), Some(id));
        }
        false
    }

    /// bspwm: `src/tree.c` `clients_count_in()`.
    pub fn clients_count_in(&self, id: Option<NodeId>) -> u32 {
        let Some(id) = id else { return 0 };
        let node = self.node(id);
        (node.client.is_some() as u32)
            + self.clients_count_in(node.first_child)
            + self.clients_count_in(node.second_child)
    }

    /// bspwm: `src/tree.c` `tiled_count()`.
    pub fn tiled_count(&self, id: Option<NodeId>, include_receptacles: bool) -> i32 {
        let mut count = 0;
        let mut f = self.first_extrema(id);
        while let Some(n) = f {
            let node = self.node(n);
            let counts = !node.hidden
                && ((include_receptacles && node.client.is_none())
                    || node.client.as_ref().is_some_and(|c| c.state.is_tiled()));
            if counts {
                count += 1;
            }
            f = self.next_leaf(Some(n), id);
        }
        count
    }

    fn flag_count(&self, id: Option<NodeId>, flag: impl Fn(&Node) -> bool + Copy) -> u32 {
        let Some(id) = id else { return 0 };
        let node = self.node(id);
        (flag(node) as u32)
            + self.flag_count(node.first_child, flag)
            + self.flag_count(node.second_child, flag)
    }

    /// bspwm: `src/tree.c` `sticky_count()`.
    pub fn sticky_count(&self, id: Option<NodeId>) -> u32 {
        self.flag_count(id, |n| n.sticky)
    }

    /// bspwm: `src/tree.c` `private_count()`.
    pub fn private_count(&self, id: Option<NodeId>) -> u32 {
        self.flag_count(id, |n| n.private)
    }

    /// bspwm: `src/tree.c` `locked_count()`.
    pub fn locked_count(&self, id: Option<NodeId>) -> u32 {
        self.flag_count(id, |n| n.locked)
    }

    /// The rectangle a client-less node should be measured with: its raw
    /// layout rectangle, shrunk by the window gap.
    ///
    /// bspwm: `src/tree.c` `get_rectangle()`. A client node instead
    /// reports whichever of `floating_rectangle`/`tiled_rectangle` its
    /// state selects.
    pub fn get_rectangle(&self, id: NodeId, window_gap: i32, layout: Layout) -> Rect {
        let node = self.node(id);
        if let Some(c) = &node.client {
            return if c.state == ClientState::Floating {
                c.floating_rectangle
            } else {
                c.tiled_rectangle
            };
        }
        let wg = if layout == Layout::Monocle {
            0
        } else {
            window_gap
        };
        let mut r = node.rect;
        r.width -= wg;
        r.height -= wg;
        r
    }

    /// bspwm: `src/tree.c` `node_area()`, simplified to the node's raw
    /// layout rectangle rather than routing through `get_rectangle()`: the
    /// only caller, [`Tree::find_public`], uses the value solely to rank
    /// leaves against each other, and every leaf is measured the same way.
    pub fn node_area(&self, id: NodeId) -> i64 {
        self.node(id).rect.area()
    }

    // ---- Preselection -----------------------------------------------

    /// bspwm: `src/tree.c` `presel_dir()`.
    pub fn presel_dir(&mut self, id: NodeId, dir: Direction, default_ratio: f64) {
        let node = self.node_mut(id);
        match &mut node.presel {
            Some(p) => p.split_dir = dir,
            None => {
                node.presel = Some(Presel {
                    split_ratio: default_ratio,
                    split_dir: dir,
                })
            }
        }
    }

    /// bspwm: `src/tree.c` `presel_ratio()`.
    pub fn presel_ratio(&mut self, id: NodeId, ratio: f64, default_dir: Direction) {
        let node = self.node_mut(id);
        match &mut node.presel {
            Some(p) => p.split_ratio = ratio,
            None => {
                node.presel = Some(Presel {
                    split_ratio: ratio,
                    split_dir: default_dir,
                })
            }
        }
    }

    /// bspwm: `src/tree.c` `cancel_presel()`.
    pub fn cancel_presel(&mut self, id: NodeId) {
        self.node_mut(id).presel = None;
    }

    /// bspwm: `src/tree.c` `cancel_presel_in()`.
    pub fn cancel_presel_in(&mut self, id: Option<NodeId>) {
        let Some(id) = id else { return };
        self.cancel_presel(id);
        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        self.cancel_presel_in(first);
        self.cancel_presel_in(second);
    }

    // ---- Vacancy and hidden propagation ------------------------------

    fn set_vacant_local(&mut self, id: NodeId, value: bool) {
        if self.node(id).vacant == value {
            return;
        }
        self.node_mut(id).vacant = value;
        if value {
            self.cancel_presel(id);
        }
    }

    fn propagate_vacant_downward(&mut self, id: Option<NodeId>, value: bool) {
        let Some(id) = id else { return };
        self.set_vacant_local(id, value);
        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        self.propagate_vacant_downward(first, value);
        self.propagate_vacant_downward(second, value);
    }

    fn propagate_vacant_upward(&mut self, id: Option<NodeId>) {
        let Some(id) = id else { return };
        let parent = self.node(id).parent;
        if let Some(p) = parent {
            let both_vacant = {
                let pn = self.node(p);
                self.node(pn.first_child.unwrap()).vacant
                    && self.node(pn.second_child.unwrap()).vacant
            };
            self.set_vacant_local(p, both_vacant);
        }
        self.propagate_vacant_upward(parent);
    }

    /// bspwm: `src/tree.c` `set_vacant()`.
    pub fn set_vacant(&mut self, id: NodeId, value: bool) {
        if self.node(id).vacant == value {
            return;
        }
        self.propagate_vacant_downward(Some(id), value);
        self.propagate_vacant_upward(Some(id));
    }

    fn set_hidden_local(&mut self, id: NodeId, value: bool) {
        if self.node(id).hidden == value {
            return;
        }
        self.node_mut(id).hidden = value;
        let is_tiled = self
            .node(id)
            .client
            .as_ref()
            .is_some_and(|c| c.state.is_tiled());
        if is_tiled {
            self.set_vacant(id, value);
        }
    }

    fn propagate_hidden_downward(&mut self, id: Option<NodeId>, value: bool) {
        let Some(id) = id else { return };
        self.set_hidden_local(id, value);
        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        self.propagate_hidden_downward(first, value);
        self.propagate_hidden_downward(second, value);
    }

    fn propagate_hidden_upward(&mut self, id: Option<NodeId>) {
        let Some(id) = id else { return };
        let parent = self.node(id).parent;
        if let Some(p) = parent {
            let both_hidden = {
                let pn = self.node(p);
                self.node(pn.first_child.unwrap()).hidden
                    && self.node(pn.second_child.unwrap()).hidden
            };
            self.set_hidden_local(p, both_hidden);
        }
        self.propagate_hidden_upward(parent);
    }

    /// bspwm: `src/tree.c` `set_hidden()`, minus the focus reassignment
    /// bspwm performs inline (`activate_node`/`focus_node`): that needs a
    /// monitor and is the adapter's job once hidden windows can be
    /// un-hidden interactively.
    pub fn set_hidden(&mut self, id: NodeId, value: bool) {
        if self.node(id).hidden == value {
            return;
        }
        self.propagate_hidden_downward(Some(id), value);
        self.propagate_hidden_upward(Some(id));
    }

    fn update_constraints(&mut self, id: NodeId) {
        if self.is_leaf(id) {
            return;
        }
        let (first, second, split_type) = {
            let n = self.node(id);
            (
                n.first_child.unwrap(),
                n.second_child.unwrap(),
                n.split_type,
            )
        };
        let fc = self.node(first).constraints;
        let sc = self.node(second).constraints;
        let constraints = if split_type == SplitType::Vertical {
            Constraints {
                min_width: fc.min_width + sc.min_width,
                min_height: fc.min_height.max(sc.min_height),
            }
        } else {
            Constraints {
                min_width: fc.min_width.max(sc.min_width),
                min_height: fc.min_height + sc.min_height,
            }
        };
        self.node_mut(id).constraints = constraints;
    }

    /// bspwm: `src/tree.c` `rebuild_constraints_from_leaves()`.
    pub fn rebuild_constraints_from_leaves(&mut self, id: Option<NodeId>) {
        let Some(id) = id else { return };
        if self.is_leaf(id) {
            return;
        }
        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        self.rebuild_constraints_from_leaves(first);
        self.rebuild_constraints_from_leaves(second);
        self.update_constraints(id);
    }

    /// bspwm: `src/tree.c` `rebuild_constraints_towards_root()`.
    pub fn rebuild_constraints_towards_root(&mut self, id: Option<NodeId>) {
        let Some(id) = id else { return };
        let parent = self.node(id).parent;
        if let Some(p) = parent {
            self.update_constraints(p);
        }
        self.rebuild_constraints_towards_root(parent);
    }

    fn propagate_flags_upward(&mut self, id: Option<NodeId>) {
        let Some(id) = id else { return };
        let parent = self.node(id).parent;
        if let Some(p) = parent {
            let (fc, sc) = {
                let pn = self.node(p);
                (pn.first_child.unwrap(), pn.second_child.unwrap())
            };
            let vacant = self.node(fc).vacant && self.node(sc).vacant;
            self.set_vacant_local(p, vacant);
            let hidden = self.node(fc).hidden && self.node(sc).hidden;
            self.set_hidden_local(p, hidden);
            self.update_constraints(p);
        }
        self.propagate_flags_upward(parent);
    }

    // ---- Node flag setters -------------------------------------------

    /// bspwm: `src/tree.c` `set_sticky()`, minus the monitor sticky-count
    /// bookkeeping (`crate::monitor` owns that).
    pub fn set_sticky(&mut self, id: NodeId, value: bool) {
        self.node_mut(id).sticky = value;
    }

    /// bspwm: `src/tree.c` `set_private()`.
    pub fn set_private(&mut self, id: NodeId, value: bool) {
        self.node_mut(id).private = value;
    }

    /// bspwm: `src/tree.c` `set_locked()`.
    pub fn set_locked(&mut self, id: NodeId, value: bool) {
        self.node_mut(id).locked = value;
    }

    /// bspwm: `src/tree.c` `set_marked()`.
    pub fn set_marked(&mut self, id: NodeId, value: bool) {
        self.node_mut(id).marked = value;
    }

    /// bspwm: `src/tree.c` `set_urgent()`. Returns `false` if `id` has no
    /// client.
    pub fn set_urgent(&mut self, id: NodeId, value: bool) -> bool {
        match &mut self.node_mut(id).client {
            Some(c) => {
                c.urgent = value;
                true
            }
            None => false,
        }
    }

    /// bspwm: `src/tree.c` `set_layer()`. Returns `false` if `id` has no
    /// client or is already on layer `l`.
    pub fn set_layer(&mut self, id: NodeId, l: Layer) -> bool {
        match &mut self.node_mut(id).client {
            Some(c) if c.layer != l => {
                c.last_layer = c.layer;
                c.layer = l;
                true
            }
            _ => false,
        }
    }

    /// bspwm: `src/tree.c` `set_floating()`, minus stacking.
    pub fn set_floating(&mut self, id: NodeId, value: bool) {
        self.cancel_presel(id);
        if !self.node(id).hidden {
            self.set_vacant(id, value);
        }
    }

    /// bspwm: `src/tree.c` `set_fullscreen()`, minus stacking and EWMH.
    pub fn set_fullscreen(&mut self, id: NodeId, value: bool) {
        self.cancel_presel(id);
        if !self.node(id).hidden {
            self.set_vacant(id, value);
        }
    }

    /// bspwm: `src/tree.c` `set_state()`, minus `single_monocle` and the
    /// `subscribe` report (desktop-level concerns). Returns `false` if
    /// `id` has no client or is already in state `s`.
    pub fn set_state(&mut self, id: NodeId, s: ClientState) -> bool {
        let Some(client) = &self.node(id).client else {
            return false;
        };
        if client.state == s {
            return false;
        }
        let last_state = client.state;

        match last_state {
            ClientState::Tiled | ClientState::PseudoTiled => {}
            ClientState::Floating => self.set_floating(id, false),
            ClientState::Fullscreen => self.set_fullscreen(id, false),
        }

        {
            let c = self.node_mut(id).client.as_mut().unwrap();
            c.last_state = last_state;
            c.state = s;
        }

        match s {
            ClientState::Tiled | ClientState::PseudoTiled => {}
            ClientState::Floating => self.set_floating(id, true),
            ClientState::Fullscreen => self.set_fullscreen(id, true),
        }

        true
    }

    // ---- Insertion and removal ----------------------------------------

    /// Finds the best receptacle for an automatic insertion that landed on
    /// a private node: the leaf with the largest area that is not private
    /// (preferring one with no private ancestor and no pending
    /// preselection), or `None` if the desktop holds no such leaf.
    ///
    /// bspwm: `src/tree.c` `find_public()`.
    pub fn find_public(&self, root: Option<NodeId>) -> Option<NodeId> {
        let mut best_manual: Option<(NodeId, i64)> = None;
        let mut best_automatic: Option<(NodeId, i64)> = None;
        let mut f = self.first_extrema(root);
        while let Some(n) = f {
            let node = self.node(n);
            if !node.vacant {
                let area = self.node_area(n);
                if (node.presel.is_some() || !node.private)
                    && best_manual.is_none_or(|(_, a)| area > a)
                {
                    best_manual = Some((n, area));
                }
                let parent_private = node.parent.is_some_and(|p| self.private_count(Some(p)) > 0);
                if node.presel.is_none()
                    && !node.private
                    && !parent_private
                    && best_automatic.is_none_or(|(_, a)| area > a)
                {
                    best_automatic = Some((n, area));
                }
            }
            f = self.next_leaf(Some(n), root);
        }
        best_automatic.or(best_manual).map(|(n, _)| n)
    }

    /// Inserts node `n` (freshly made with [`Tree::new_node`] or
    /// [`Tree::new_client_node`]) next to anchor `f` (or at the root if
    /// `f` is `None`), splitting `f`'s slot in two unless `f` is an empty
    /// receptacle, in which case `n` simply replaces it. Returns the
    /// anchor actually used (which can differ from `f` when `f` was
    /// private and insertion was redirected by [`Tree::find_public`]).
    ///
    /// bspwm: `src/tree.c` `insert_node()`, minus the `subscribe` report.
    pub fn insert_node(
        &mut self,
        settings: &Settings,
        n: NodeId,
        f: Option<NodeId>,
    ) -> Option<NodeId> {
        let mut f = f.or(self.root);

        match f {
            None => {
                self.root = Some(n);
            }
            Some(f_id) if self.is_receptacle(f_id) && self.node(f_id).presel.is_none() => {
                let p = self.node(f_id).parent;
                match p {
                    Some(p) => {
                        if self.node(p).first_child == Some(f_id) {
                            self.node_mut(p).first_child = Some(n);
                        } else {
                            self.node_mut(p).second_child = Some(n);
                        }
                    }
                    None => self.root = Some(n),
                }
                self.node_mut(n).parent = p;
                self.free_slot(f_id);
                f = None;
            }
            Some(mut f_id) => {
                let mut p = self.node(f_id).parent;
                if self.node(f_id).presel.is_none()
                    && (self.node(f_id).private
                        || p.is_some_and(|p| self.private_count(Some(p)) > 0))
                {
                    if let Some(k) = self.find_public(self.root) {
                        f_id = k;
                        p = self.node(f_id).parent;
                    }
                    if self.node(f_id).presel.is_none()
                        && (self.node(f_id).private
                            || p.is_some_and(|p| self.private_count(Some(p)) > 0))
                    {
                        let rect = self.node(f_id).rect;
                        let dir = if rect.width >= rect.height {
                            Direction::East
                        } else {
                            Direction::South
                        };
                        self.presel_dir(f_id, dir, settings.split_ratio);
                    }
                }

                let c = self.new_node(settings);
                self.node_mut(n).parent = Some(c);

                if self.node(f_id).presel.is_none() {
                    let single_tiled = self
                        .node(f_id)
                        .client
                        .as_ref()
                        .is_some_and(|cl| cl.state.is_tiled())
                        && self.tiled_count(self.root, true) == 1;

                    if p.is_none()
                        || settings.automatic_scheme != AutomaticScheme::Spiral
                        || single_tiled
                    {
                        match p {
                            Some(p_id) => {
                                if self.node(p_id).first_child == Some(f_id) {
                                    self.node_mut(p_id).first_child = Some(c);
                                } else {
                                    self.node_mut(p_id).second_child = Some(c);
                                }
                            }
                            None => self.root = Some(c),
                        }
                        self.node_mut(c).parent = p;
                        self.node_mut(f_id).parent = Some(c);
                        if settings.initial_polarity == ChildPolarity::First {
                            self.node_mut(c).first_child = Some(n);
                            self.node_mut(c).second_child = Some(f_id);
                        } else {
                            self.node_mut(c).first_child = Some(f_id);
                            self.node_mut(c).second_child = Some(n);
                        }

                        let split_type = if p.is_none()
                            || settings.automatic_scheme == AutomaticScheme::LongestSide
                            || single_tiled
                        {
                            let r = self.node(f_id).rect;
                            if r.width > r.height {
                                SplitType::Vertical
                            } else {
                                SplitType::Horizontal
                            }
                        } else {
                            let mut q = p;
                            while let Some(q_id) = q {
                                let node = self.node(q_id);
                                let (fc, sc) =
                                    (node.first_child.unwrap(), node.second_child.unwrap());
                                if self.node(fc).vacant || self.node(sc).vacant {
                                    q = self.node(q_id).parent;
                                } else {
                                    break;
                                }
                            }
                            let q = q.or(p).unwrap();
                            if self.node(q).split_type == SplitType::Horizontal {
                                SplitType::Vertical
                            } else {
                                SplitType::Horizontal
                            }
                        };
                        self.node_mut(c).split_type = split_type;
                    } else {
                        // SCHEME_SPIRAL, real parent, not the sole tiled window:
                        // the enclosing `if`'s condition (`p.is_none() || ...`)
                        // was false to reach this branch, so `p` is `Some`.
                        let Some(p_id) = p else {
                            unreachable!("p is Some: see comment above")
                        };
                        let g = self.node(p_id).parent;
                        self.node_mut(c).parent = g;
                        match g {
                            Some(g_id) => {
                                if self.node(g_id).first_child == Some(p_id) {
                                    self.node_mut(g_id).first_child = Some(c);
                                } else {
                                    self.node_mut(g_id).second_child = Some(c);
                                }
                            }
                            None => self.root = Some(c),
                        }
                        self.node_mut(c).split_type = self.node(p_id).split_type;
                        self.node_mut(c).split_ratio = self.node(p_id).split_ratio;
                        self.node_mut(p_id).parent = Some(c);

                        let rot = if self.is_first_child(f_id) {
                            self.node_mut(c).first_child = Some(n);
                            self.node_mut(c).second_child = Some(p_id);
                            90
                        } else {
                            self.node_mut(c).first_child = Some(p_id);
                            self.node_mut(c).second_child = Some(n);
                            270
                        };
                        if !self.node(n).vacant {
                            self.rotate_tree(Some(p_id), rot);
                        }
                    }
                } else {
                    if let Some(p_id) = p {
                        if self.node(p_id).first_child == Some(f_id) {
                            self.node_mut(p_id).first_child = Some(c);
                        } else {
                            self.node_mut(p_id).second_child = Some(c);
                        }
                    }
                    let presel = self.node(f_id).presel.unwrap();
                    self.node_mut(c).split_ratio = presel.split_ratio;
                    self.node_mut(c).parent = p;
                    self.node_mut(f_id).parent = Some(c);
                    match presel.split_dir {
                        Direction::West => {
                            self.node_mut(c).split_type = SplitType::Vertical;
                            self.node_mut(c).first_child = Some(n);
                            self.node_mut(c).second_child = Some(f_id);
                        }
                        Direction::East => {
                            self.node_mut(c).split_type = SplitType::Vertical;
                            self.node_mut(c).first_child = Some(f_id);
                            self.node_mut(c).second_child = Some(n);
                        }
                        Direction::North => {
                            self.node_mut(c).split_type = SplitType::Horizontal;
                            self.node_mut(c).first_child = Some(n);
                            self.node_mut(c).second_child = Some(f_id);
                        }
                        Direction::South => {
                            self.node_mut(c).split_type = SplitType::Horizontal;
                            self.node_mut(c).first_child = Some(f_id);
                            self.node_mut(c).second_child = Some(n);
                        }
                    }
                    if self.root == Some(f_id) {
                        self.root = Some(c);
                    }
                    self.cancel_presel(f_id);
                    self.set_marked(n, false);
                }
            }
        }

        self.rebuild_constraints_from_leaves(Some(n));
        self.rebuild_constraints_towards_root(Some(n));
        self.propagate_flags_upward(Some(n));

        if self.focus.is_none() && self.is_focusable(n) {
            self.focus = Some(n);
        }

        f
    }

    /// Removes `n` from the tree structurally: `n`'s sibling takes over
    /// `n`'s parent's slot. `n` itself is left allocated (callers that
    /// also want to free it call [`Tree::free_node`]); this split mirrors
    /// bspwm's own `unlink_node()`/`free_node()` split, which
    /// `transplant_to` relies on to move a node without freeing it.
    ///
    /// bspwm: `src/tree.c` `unlink_node()`, minus history, presel
    /// feedback windows and monitor sticky-count bookkeeping.
    pub fn unlink_node(&mut self, settings: &Settings, n: NodeId) {
        let p = match self.node(n).parent {
            Some(p) => p,
            None => {
                self.root = None;
                self.focus = None;
                return;
            }
        };

        if self.focus == Some(p) || self.is_descendant(self.focus, Some(n)) {
            self.focus = None;
        }
        self.cancel_presel(p);

        let b = self
            .brother(n)
            .expect("internal node always has two children");
        let g = self.node(p).parent;
        self.node_mut(b).parent = g;

        match g {
            Some(g_id) => {
                if self.node(g_id).first_child == Some(p) {
                    self.node_mut(g_id).first_child = Some(b);
                } else {
                    self.node_mut(g_id).second_child = Some(b);
                }
            }
            None => self.root = Some(b),
        }

        if !self.node(n).vacant && settings.removal_adjustment {
            match settings.automatic_scheme {
                AutomaticScheme::Spiral => {
                    if self.is_first_child(n) {
                        self.rotate_tree(Some(b), 270);
                    } else {
                        self.rotate_tree(Some(b), 90);
                    }
                }
                AutomaticScheme::LongestSide => {
                    let r = self.node(p).rect;
                    self.node_mut(b).split_type = if r.width > r.height {
                        SplitType::Vertical
                    } else {
                        SplitType::Horizontal
                    };
                }
                AutomaticScheme::Alternate => match g {
                    Some(g_id) => {
                        self.node_mut(b).split_type =
                            if self.node(g_id).split_type == SplitType::Horizontal {
                                SplitType::Vertical
                            } else {
                                SplitType::Horizontal
                            };
                    }
                    None => {
                        let r = self.node(p).rect;
                        self.node_mut(b).split_type = if r.width > r.height {
                            SplitType::Vertical
                        } else {
                            SplitType::Horizontal
                        };
                    }
                },
            }
        }

        self.free_slot(p);
        self.node_mut(n).parent = None;

        self.propagate_flags_upward(Some(b));
    }

    fn free_slot(&mut self, id: NodeId) {
        self.nodes[id.index()] = None;
        self.free.push(id.0);
    }

    /// Frees `n` and its whole subtree. Call after [`Tree::unlink_node`]
    /// to fully remove a node, or directly on a node that was never
    /// linked in (e.g. to discard a [`Tree::new_node`] that turned out
    /// unneeded).
    ///
    /// bspwm: `src/tree.c` `free_node()`.
    pub fn free_node(&mut self, n: NodeId) {
        let (first, second) = {
            let node = self.node(n);
            (node.first_child, node.second_child)
        };
        self.free_slot(n);
        if let Some(first) = first {
            self.free_node(first);
        }
        if let Some(second) = second {
            self.free_node(second);
        }
    }

    /// Removes `n` from the tree and frees it and its subtree.
    ///
    /// bspwm: `src/tree.c` `remove_node()`, minus history, the stacking
    /// list, `single_monocle`, EWMH and refocusing: all belong to
    /// `crate::desktop`/the adapter, which see the freed node's former
    /// position (via `unlink_node`) rather than repeating this call's
    /// logic.
    pub fn remove_node(&mut self, settings: &Settings, n: NodeId) {
        self.unlink_node(settings, n);
        self.free_node(n);
    }

    // ---- Rotation, flipping, balancing ---------------------------------

    fn rotate_tree_rec(&mut self, id: Option<NodeId>, deg: i32) {
        let Some(id) = id else { return };
        if self.is_leaf(id) || deg == 0 {
            return;
        }

        let split_type = self.node(id).split_type;
        let swap = (deg == 90 && split_type == SplitType::Horizontal)
            || (deg == 270 && split_type == SplitType::Vertical)
            || deg == 180;

        if swap {
            let n = self.node_mut(id);
            let (fc, sc) = (n.first_child, n.second_child);
            n.first_child = sc;
            n.second_child = fc;
            n.split_ratio = 1.0 - n.split_ratio;
        }

        if deg != 180 {
            let n = self.node_mut(id);
            n.split_type = match n.split_type {
                SplitType::Horizontal => SplitType::Vertical,
                SplitType::Vertical => SplitType::Horizontal,
            };
        }

        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        self.rotate_tree_rec(first, deg);
        self.rotate_tree_rec(second, deg);
    }

    /// Rotates the subtree rooted at `id` by `deg` degrees (90, 180 or
    /// 270; any other value is a no-op).
    ///
    /// bspwm: `src/tree.c` `rotate_tree()`/`rotate_tree_rec()`.
    pub fn rotate_tree(&mut self, id: Option<NodeId>, deg: i32) {
        self.rotate_tree_rec(id, deg);
        self.rebuild_constraints_from_leaves(id);
        self.rebuild_constraints_towards_root(id);
    }

    /// Mirrors the subtree rooted at `id` across `axis`.
    ///
    /// bspwm: `src/tree.c` `flip_tree()`.
    pub fn flip_tree(&mut self, id: Option<NodeId>, axis: FlipAxis) {
        let Some(id) = id else { return };
        if self.is_leaf(id) {
            return;
        }

        let split_type = self.node(id).split_type;
        let swap = (axis == FlipAxis::Horizontal && split_type == SplitType::Horizontal)
            || (axis == FlipAxis::Vertical && split_type == SplitType::Vertical);

        if swap {
            let n = self.node_mut(id);
            let (fc, sc) = (n.first_child, n.second_child);
            n.first_child = sc;
            n.second_child = fc;
            n.split_ratio = 1.0 - n.split_ratio;
        }

        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        self.flip_tree(first, axis);
        self.flip_tree(second, axis);
    }

    /// Resets every split ratio in the subtree rooted at `id` to
    /// `settings.split_ratio`.
    ///
    /// bspwm: `src/tree.c` `equalize_tree()`.
    pub fn equalize_tree(&mut self, id: Option<NodeId>, settings: &Settings) {
        let Some(id) = id else { return };
        if self.node(id).vacant {
            return;
        }
        self.node_mut(id).split_ratio = settings.split_ratio;
        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        self.equalize_tree(first, settings);
        self.equalize_tree(second, settings);
    }

    /// Sets every split ratio in the subtree rooted at `id` so that each
    /// leaf gets equal space, and returns the number of non-vacant leaves
    /// found.
    ///
    /// bspwm: `src/tree.c` `balance_tree()`.
    pub fn balance_tree(&mut self, id: Option<NodeId>) -> i32 {
        let Some(id) = id else { return 0 };
        if self.node(id).vacant {
            return 0;
        }
        if self.is_leaf(id) {
            return 1;
        }
        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        let b1 = self.balance_tree(first);
        let b2 = self.balance_tree(second);
        let b = b1 + b2;
        if b1 > 0 && b2 > 0 {
            self.node_mut(id).split_ratio = b1 as f64 / b as f64;
        }
        b
    }

    /// Adjusts split ratios in the subtree rooted at `id` so that every
    /// fence keeps its pixel position when the subtree's rectangle changes
    /// from its last computed one to `rect`.
    ///
    /// bspwm: `src/tree.c` `adjust_ratios()`.
    pub fn adjust_ratios(&mut self, id: Option<NodeId>, rect: Rect) {
        let Some(id) = id else { return };
        if self.node(id).vacant {
            return;
        }

        let node = self.node(id);
        let ratio = if node.split_type == SplitType::Vertical {
            let position = node.rect.x as f64 + node.split_ratio * node.rect.width as f64;
            (position - rect.x as f64) / rect.width as f64
        } else {
            let position = node.rect.y as f64 + node.split_ratio * node.rect.height as f64;
            (position - rect.y as f64) / rect.height as f64
        };
        let ratio = ratio.clamp(0.0, 1.0);
        self.node_mut(id).split_ratio = ratio;

        let (first, second) = {
            let n = self.node(id);
            (n.first_child, n.second_child)
        };
        let first_vacant = first.is_none_or(|f| self.node(f).vacant);
        let second_vacant = second.is_none_or(|s| self.node(s).vacant);
        if first_vacant {
            self.adjust_ratios(second, rect);
            return;
        }
        if second_vacant {
            self.adjust_ratios(first, rect);
            return;
        }

        let split_type = self.node(id).split_type;
        let (first_rect, second_rect) = if split_type == SplitType::Vertical {
            let fence = (rect.width as f64 * ratio) as i32;
            (
                Rect::new(rect.x, rect.y, fence, rect.height),
                Rect::new(rect.x + fence, rect.y, rect.width - fence, rect.height),
            )
        } else {
            let fence = (rect.height as f64 * ratio) as i32;
            (
                Rect::new(rect.x, rect.y, rect.width, fence),
                Rect::new(rect.x, rect.y + fence, rect.width, rect.height - fence),
            )
        };
        self.adjust_ratios(first, first_rect);
        self.adjust_ratios(second, second_rect);
    }

    // ---- Swap and transplant ------------------------------------------

    /// Swaps the positions of `n1` and `n2` in the tree: each takes over
    /// the other's parent slot. Fails (returning `false`) if either node
    /// is an ancestor of the other, or they are the same node. Because
    /// everything is a descendant of the root, that guard also means
    /// neither `n1` nor `n2` can be the root, so `self.root` never needs
    /// updating here (bspwm's `swap_nodes()` only reassigns `d->root` in
    /// its cross-desktop branch, for exactly this reason). `self.focus`
    /// likewise never needs updating: focus is tracked by node identity,
    /// and this only changes where `n1`/`n2` sit, not which `NodeId` they
    /// are.
    ///
    /// bspwm: `src/tree.c` `swap_nodes()`, restricted to a single tree
    /// (see [`transplant_to`](Tree::transplant_to) for moving a node to a
    /// different desktop's tree).
    pub fn swap_nodes(&mut self, n1: NodeId, n2: NodeId) -> bool {
        if n1 == n2
            || self.is_descendant(Some(n1), Some(n2))
            || self.is_descendant(Some(n2), Some(n1))
        {
            return false;
        }

        let pn1 = self.node(n1).parent;
        let pn2 = self.node(n2).parent;
        let n1_first_child = self.is_first_child(n1);
        let n2_first_child = self.is_first_child(n2);

        if let Some(p) = pn1 {
            if n1_first_child {
                self.node_mut(p).first_child = Some(n2);
            } else {
                self.node_mut(p).second_child = Some(n2);
            }
        }
        if let Some(p) = pn2 {
            if n2_first_child {
                self.node_mut(p).first_child = Some(n1);
            } else {
                self.node_mut(p).second_child = Some(n1);
            }
        }
        self.node_mut(n1).parent = pn2;
        self.node_mut(n2).parent = pn1;

        self.propagate_flags_upward(Some(n1));
        self.propagate_flags_upward(Some(n2));
        self.rebuild_constraints_towards_root(Some(n1));
        self.rebuild_constraints_towards_root(Some(n2));

        true
    }

    /// Clones the subtree rooted at `id` into `dest`'s arena (fresh
    /// `NodeId`s throughout: a `NodeId` is only ever meaningful within the
    /// `Tree` that issued it, see `crate::id`), preserving every field and
    /// the parent/child structure, and returns the new root's id in
    /// `dest`. `self` is left unchanged; callers that are moving rather
    /// than copying still need to unlink and free the original.
    fn clone_subtree_into(&self, id: NodeId, dest: &mut Tree) -> NodeId {
        let node = self.node(id);
        let (first, second) = (node.first_child, node.second_child);
        let mut copy = node.clone();
        copy.parent = None;
        copy.first_child = None;
        copy.second_child = None;
        let new_id = dest.alloc(copy);

        if let Some(f) = first {
            let new_f = self.clone_subtree_into(f, dest);
            dest.node_mut(new_f).parent = Some(new_id);
            dest.node_mut(new_id).first_child = Some(new_f);
        }
        if let Some(s) = second {
            let new_s = self.clone_subtree_into(s, dest);
            dest.node_mut(new_s).parent = Some(new_id);
            dest.node_mut(new_id).second_child = Some(new_s);
        }
        new_id
    }

    /// Moves `n` and its subtree from this tree into `dest`, inserting it
    /// next to `anchor` (as [`Tree::insert_node`]), and returns `n`'s new
    /// id in `dest`. `dest` must be a different `Tree` (Rust cannot borrow
    /// one tree as both `self` and `dest`, and in any case a `NodeId` is
    /// only meaningful within the arena that issued it — moving within the
    /// same tree does not need a new id); for that case, use
    /// [`Tree::transplant_within`] instead. Unlike a same-tree move, `n`
    /// grafting onto its own descendant cannot arise here (`anchor` lives
    /// in `dest`'s independent id space), so there is no failure case.
    ///
    /// This is the "transplant" tree operation from `docs/design.md`'s
    /// roadmap; it corresponds to bspwm's `transfer_node()`
    /// (`src/tree.c`), minus focus/history/EWMH/`single_monocle`, which
    /// need a monitor and belong to `crate::desktop`.
    pub fn transplant_to(
        &mut self,
        settings: &Settings,
        n: NodeId,
        dest: &mut Tree,
        anchor: Option<NodeId>,
    ) -> NodeId {
        self.unlink_node(settings, n);
        let new_id = self.clone_subtree_into(n, dest);
        self.free_node(n);
        dest.insert_node(settings, new_id, anchor);
        new_id
    }

    /// As [`Tree::transplant_to`], but within this same tree: removes `n`
    /// and re-inserts it next to `anchor`. Fails, leaving the tree
    /// unchanged, under the same condition as `transplant_to`.
    pub fn transplant_within(
        &mut self,
        settings: &Settings,
        n: NodeId,
        anchor: Option<NodeId>,
    ) -> bool {
        if anchor == Some(n) || self.is_descendant(anchor, Some(n)) {
            return false;
        }
        self.unlink_node(settings, n);
        self.insert_node(settings, n, anchor);
        true
    }

    /// bspwm: `src/tree.c` `circulate_leaves()`, minus refocusing (which
    /// needs a monitor).
    pub fn circulate_leaves(
        &mut self,
        settings: &Settings,
        root: Option<NodeId>,
        dir: CirculateDir,
    ) {
        let _ = settings;
        if self.tiled_count(root, false) < 2 {
            return;
        }

        match dir {
            CirculateDir::Forward => {
                let mut e = self.second_extrema(root);
                while let Some(n) = e {
                    let node = self.node(n);
                    if node.client.as_ref().is_some_and(|c| c.state.is_tiled()) {
                        break;
                    }
                    e = self.prev_leaf(Some(n), root);
                }
                let mut s = e;
                let mut f = s.and_then(|s| self.prev_tiled_leaf(Some(s), root));
                while let Some(f_id) = f {
                    self.swap_nodes(f_id, s.unwrap());
                    // bspwm: src/tree.c circulate_leaves()'s for-loop update
                    // clause, `s = prev_tiled_leaf(f, n), f =
                    // prev_tiled_leaf(s, n)`: both are recomputed from
                    // f_id's *post-swap* tree position, not just renamed.
                    s = self.prev_tiled_leaf(Some(f_id), root);
                    f = s.and_then(|s| self.prev_tiled_leaf(Some(s), root));
                }
            }
            CirculateDir::Backward => {
                let mut e = self.first_extrema(root);
                while let Some(n) = e {
                    let node = self.node(n);
                    if node.client.as_ref().is_some_and(|c| c.state.is_tiled()) {
                        break;
                    }
                    e = self.next_leaf(Some(n), root);
                }
                let mut f = e;
                let mut s = f.and_then(|f| self.next_tiled_leaf(Some(f), root));
                while let Some(s_id) = s {
                    self.swap_nodes(f.unwrap(), s_id);
                    // Mirrors the fix in the `Forward` arm above, for
                    // `f = next_tiled_leaf(s, n), s = next_tiled_leaf(f, n)`.
                    f = self.next_tiled_leaf(Some(s_id), root);
                    s = f.and_then(|f| self.next_tiled_leaf(Some(f), root));
                }
            }
        }
    }

    // ---- Layout ---------------------------------------------------------

    /// Computes the rectangle of every node in the subtree rooted at `id`,
    /// writing each into [`Node::rect`] (and, for tiled/pseudo-tiled/
    /// fullscreen clients, into [`Client::tiled_rectangle`]).
    ///
    /// `window_gap` and `layout` are the owning desktop's; `monitor_rect`
    /// is the owning monitor's full rectangle (used verbatim for
    /// fullscreen clients, as bspwm does). Border width and size hints
    /// (bspwm also applies `honor_size_hints` and the
    /// `borderless_singleton`/`the_only_window` special case here) are
    /// left for `bsp-compositor`: they need a real client's size hints and
    /// the full monitor list, neither of which exists yet in the core.
    ///
    /// bspwm: `src/tree.c` `apply_layout()` (the parts that compute
    /// geometry; `arrange()`, which computes the starting `rect` from a
    /// monitor and desktop's padding, is `crate::monitor::Monitor::arrange`
    /// since it needs both).
    pub fn apply_layout(
        &mut self,
        id: Option<NodeId>,
        rect: Rect,
        window_gap: i32,
        layout: Layout,
        monitor_rect: Rect,
    ) {
        let Some(id) = id else { return };
        self.node_mut(id).rect = rect;

        if self.is_leaf(id) {
            let Some(client) = self.node(id).client.clone() else {
                return;
            };

            let r = match client.state {
                ClientState::Tiled | ClientState::PseudoTiled => {
                    let wg = if layout == Layout::Monocle {
                        0
                    } else {
                        window_gap
                    };
                    let bw = client.border_width;
                    let bleed = wg + 2 * bw;
                    let mut r = rect;
                    r.width = if bleed < r.width { r.width - bleed } else { 1 };
                    r.height = if bleed < r.height {
                        r.height - bleed
                    } else {
                        1
                    };
                    if client.state == ClientState::PseudoTiled {
                        let f = client.floating_rectangle;
                        r.width = r.width.min(f.width);
                        r.height = r.height.min(f.height);
                    }
                    r
                }
                ClientState::Floating => client.floating_rectangle,
                ClientState::Fullscreen => monitor_rect,
            };

            if let Some(c) = &mut self.node_mut(id).client {
                c.tiled_rectangle = r;
            }
            return;
        }

        let (first, second, split_type, split_ratio, first_vacant, second_vacant) = {
            let n = self.node(id);
            let first = n.first_child.unwrap();
            let second = n.second_child.unwrap();
            (
                first,
                second,
                n.split_type,
                n.split_ratio,
                self.node(first).vacant,
                self.node(second).vacant,
            )
        };

        let (first_rect, second_rect) =
            if layout == Layout::Monocle || first_vacant || second_vacant {
                (rect, rect)
            } else if split_type == SplitType::Vertical {
                let fc_min = self.node(first).constraints.min_width;
                let sc_min = self.node(second).constraints.min_width;
                let mut fence = (rect.width as f64 * split_ratio) as i32;
                if fc_min + sc_min <= rect.width {
                    if fence < fc_min {
                        fence = fc_min;
                        self.node_mut(id).split_ratio = fence as f64 / rect.width as f64;
                    } else if fence > rect.width - sc_min {
                        fence = rect.width - sc_min;
                        self.node_mut(id).split_ratio = fence as f64 / rect.width as f64;
                    }
                }
                (
                    Rect::new(rect.x, rect.y, fence, rect.height),
                    Rect::new(rect.x + fence, rect.y, rect.width - fence, rect.height),
                )
            } else {
                let fc_min = self.node(first).constraints.min_height;
                let sc_min = self.node(second).constraints.min_height;
                let mut fence = (rect.height as f64 * split_ratio) as i32;
                if fc_min + sc_min <= rect.height {
                    if fence < fc_min {
                        fence = fc_min;
                        self.node_mut(id).split_ratio = fence as f64 / rect.height as f64;
                    } else if fence > rect.height - sc_min {
                        fence = rect.height - sc_min;
                        self.node_mut(id).split_ratio = fence as f64 / rect.height as f64;
                    }
                }
                (
                    Rect::new(rect.x, rect.y, rect.width, fence),
                    Rect::new(rect.x, rect.y + fence, rect.width, rect.height - fence),
                )
            };

        self.apply_layout(Some(first), first_rect, window_gap, layout, monitor_rect);
        self.apply_layout(Some(second), second_rect, window_gap, layout, monitor_rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::WindowId;
    use crate::node::Client;

    fn settings() -> Settings {
        Settings::default()
    }

    fn client(tree: &mut Tree, s: &Settings, id: u32) -> NodeId {
        tree.new_client_node(s, Client::new(WindowId(id), s.border_width))
    }

    /// Inserts a fresh client leaf next to `anchor` and returns its id.
    fn insert_client(tree: &mut Tree, s: &Settings, anchor: Option<NodeId>, id: u32) -> NodeId {
        let n = client(tree, s, id);
        tree.insert_node(s, n, anchor);
        n
    }

    fn window_of(tree: &Tree, id: NodeId) -> u32 {
        tree.node(id).client.as_ref().unwrap().window.0
    }

    // ---- insert_node (split) --------------------------------------------

    #[test]
    fn insert_into_empty_tree_becomes_root_and_focus() {
        let s = settings();
        let mut t = Tree::new();
        let n = insert_client(&mut t, &s, None, 1);
        assert_eq!(t.root, Some(n));
        assert_eq!(t.focus, Some(n));
        assert!(t.is_leaf(n));
    }

    #[test]
    fn insert_second_node_creates_a_split_with_default_polarity_second() {
        // bspwm: src/settings.c load_settings() sets
        // initial_polarity = SECOND_CHILD, so the newly inserted node
        // becomes the *second* child and the anchor keeps first_child.
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let root_before = t.root.unwrap();
        let b = insert_client(&mut t, &s, Some(a), 2);

        let root = t.root.unwrap();
        assert_ne!(
            root, root_before,
            "a new internal split node became the root"
        );
        assert!(!t.is_leaf(root));
        assert_eq!(t.node(root).first_child(), Some(a));
        assert_eq!(t.node(root).second_child(), Some(b));
        assert_eq!(t.node(a).parent(), Some(root));
        assert_eq!(t.node(b).parent(), Some(root));
    }

    #[test]
    fn insert_split_type_follows_longest_side_of_the_anchor() {
        // bspwm: src/tree.c insert_node(), the `automatic_scheme ==
        // SCHEME_LONGEST_SIDE` branch: split_type is TYPE_VERTICAL when
        // the anchor is wider than it is tall, TYPE_HORIZONTAL otherwise.
        let s = settings();

        let mut wide = Tree::new();
        let a = insert_client(&mut wide, &s, None, 1);
        wide.node_mut(a).rect = Rect::new(0, 0, 200, 100);
        insert_client(&mut wide, &s, Some(a), 2);
        assert_eq!(
            wide.node(wide.root.unwrap()).split_type,
            SplitType::Vertical
        );

        let mut tall = Tree::new();
        let a = insert_client(&mut tall, &s, None, 1);
        tall.node_mut(a).rect = Rect::new(0, 0, 100, 200);
        insert_client(&mut tall, &s, Some(a), 2);
        assert_eq!(
            tall.node(tall.root.unwrap()).split_type,
            SplitType::Horizontal
        );
    }

    #[test]
    fn insert_into_a_receptacle_replaces_it_without_a_new_split() {
        // bspwm: src/tree.c insert_node(), the `IS_RECEPTACLE(f)` branch.
        let s = settings();
        let mut t = Tree::new();
        let r = t.new_node(&s);
        t.insert_node(&s, r, None);
        assert_eq!(t.root, Some(r));

        let c = client(&mut t, &s, 1);
        let anchor = t.insert_node(&s, c, Some(r));
        assert_eq!(
            anchor, None,
            "the receptacle was consumed, not kept as an anchor"
        );
        assert_eq!(
            t.root,
            Some(c),
            "the client took the receptacle's exact slot"
        );
        assert!(!t.contains(r), "the receptacle was freed");
    }

    #[test]
    fn insert_with_presel_places_the_new_node_on_the_requested_side() {
        // bspwm: src/tree.c insert_node(), the `f->presel != NULL` branch
        // and its DIR_WEST/DIR_WEST case in particular.
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        t.presel_dir(a, Direction::West, 0.5);

        let b = insert_client(&mut t, &s, Some(a), 2);

        let root = t.root.unwrap();
        assert_eq!(t.node(root).split_type, SplitType::Vertical);
        assert_eq!(
            t.node(root).first_child(),
            Some(b),
            "West: new node goes first"
        );
        assert_eq!(t.node(root).second_child(), Some(a));
        assert!(
            t.node(a).presel.is_none(),
            "presel is consumed by the insert"
        );
    }

    #[test]
    fn insert_with_presel_east_keeps_anchor_first() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        t.presel_dir(a, Direction::East, 0.5);
        let b = insert_client(&mut t, &s, Some(a), 2);

        let root = t.root.unwrap();
        assert_eq!(t.node(root).first_child(), Some(a));
        assert_eq!(t.node(root).second_child(), Some(b));
    }

    #[test]
    fn insert_avoids_a_private_node_when_a_public_leaf_exists() {
        // bspwm: src/tree.c insert_node() redirects automatic insertion
        // away from a private anchor via find_public().
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        t.node_mut(a).rect = Rect::new(0, 0, 100, 100);
        t.node_mut(b).rect = Rect::new(100, 0, 50, 50);
        t.set_private(a, true);

        // Automatic insertion anchored on the private node `a` should
        // redirect to the public node `b` instead of splitting `a`.
        let c = client(&mut t, &s, 3);
        t.insert_node(&s, c, Some(a));

        assert_eq!(t.node(b).parent(), t.node(c).parent());
        assert_ne!(t.node(a).parent(), t.node(c).parent());
    }

    // ---- remove_node / unlink_node --------------------------------------

    #[test]
    fn remove_last_node_empties_the_tree() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        t.remove_node(&s, a);
        assert_eq!(t.root, None);
        assert_eq!(t.focus, None);
    }

    #[test]
    fn remove_a_leaf_promotes_its_sibling_into_the_parent_slot() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();

        t.remove_node(&s, b);

        assert_eq!(t.root, Some(a), "a's sibling (a) replaces the freed split");
        assert!(!t.contains(root), "the internal split node was freed");
        assert_eq!(t.node(a).parent(), None);
    }

    #[test]
    fn remove_adjustment_longest_side_updates_the_siblings_split_type() {
        // bspwm: src/tree.c unlink_node(), SCHEME_LONGEST_SIDE branch:
        // after a removal, the promoted sibling's split_type is set from
        // the aspect ratio of the parent slot it now fully occupies.
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1); // becomes first_child
        let b = insert_client(&mut t, &s, Some(a), 2); // second_child
        let c = insert_client(&mut t, &s, Some(b), 3); // splits b

        let p = t.node(b).parent().unwrap(); // the split created for b/c
        t.node_mut(p).rect = Rect::new(0, 0, 50, 200); // taller than wide

        t.remove_node(&s, c);

        // b now solely occupies p's old rectangle; LongestSide picks
        // Horizontal for a taller-than-wide rectangle.
        assert_eq!(t.node(b).split_type, SplitType::Horizontal);
    }

    #[test]
    fn removal_adjustment_false_leaves_siblings_split_type_untouched() {
        let mut s = settings();
        s.removal_adjustment = false;
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let c = insert_client(&mut t, &s, Some(b), 3);
        t.node_mut(b).split_type = SplitType::Vertical;
        let p = t.node(b).parent().unwrap();
        t.node_mut(p).rect = Rect::new(0, 0, 50, 200);

        t.remove_node(&s, c);

        assert_eq!(
            t.node(b).split_type,
            SplitType::Vertical,
            "untouched by removal"
        );
    }

    // ---- rotate_tree ------------------------------------------------------

    #[test]
    fn rotate_180_swaps_children_and_inverts_ratio_but_keeps_split_type() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        t.node_mut(root).split_ratio = 0.3;
        let split_type = t.node(root).split_type;

        t.rotate_tree(Some(root), 180);

        assert_eq!(t.node(root).first_child(), Some(b));
        assert_eq!(t.node(root).second_child(), Some(a));
        assert!((t.node(root).split_ratio - 0.7).abs() < 1e-9);
        assert_eq!(
            t.node(root).split_type,
            split_type,
            "180 degrees keeps orientation"
        );
    }

    #[test]
    fn rotate_90_on_a_horizontal_split_swaps_children_and_flips_to_vertical() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        t.node_mut(root).split_type = SplitType::Horizontal;

        t.rotate_tree(Some(root), 90);

        assert_eq!(t.node(root).split_type, SplitType::Vertical);
        assert_eq!(t.node(root).first_child(), Some(b));
        assert_eq!(t.node(root).second_child(), Some(a));
    }

    #[test]
    fn rotate_90_on_a_vertical_split_flips_type_without_swapping_children() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        t.node_mut(root).split_type = SplitType::Vertical;

        t.rotate_tree(Some(root), 90);

        assert_eq!(t.node(root).split_type, SplitType::Horizontal);
        assert_eq!(t.node(root).first_child(), Some(a));
        assert_eq!(t.node(root).second_child(), Some(b));
    }

    #[test]
    fn rotate_270_is_the_inverse_of_rotate_90() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let _b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        let before = t.node(root).clone();

        t.rotate_tree(Some(root), 90);
        t.rotate_tree(Some(root), 270);

        let after = t.node(root).clone();
        assert_eq!(before.split_type, after.split_type);
        assert_eq!(before.first_child(), after.first_child());
        assert_eq!(before.second_child(), after.second_child());
        assert!((before.split_ratio - after.split_ratio).abs() < 1e-9);
    }

    // ---- flip_tree ----------------------------------------------------

    #[test]
    fn flip_horizontal_swaps_only_horizontal_splits() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        t.node_mut(root).split_type = SplitType::Vertical;

        t.flip_tree(Some(root), FlipAxis::Horizontal);

        assert_eq!(
            t.node(root).first_child(),
            Some(a),
            "vertical split untouched by a horizontal flip"
        );
        assert_eq!(
            t.node(root).split_type,
            SplitType::Vertical,
            "flip never changes split_type"
        );

        t.flip_tree(Some(root), FlipAxis::Vertical);
        assert_eq!(
            t.node(root).first_child(),
            Some(b),
            "vertical split IS mirrored by a vertical flip"
        );
    }

    // ---- equalize_tree / balance_tree -----------------------------------

    #[test]
    fn equalize_resets_every_ratio_to_the_default() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let _c = insert_client(&mut t, &s, Some(b), 3);
        let root = t.root.unwrap();
        t.node_mut(root).split_ratio = 0.9;
        let p = t.node(b).parent().unwrap();
        t.node_mut(p).split_ratio = 0.1;

        t.equalize_tree(Some(root), &s);

        assert_eq!(t.node(root).split_ratio, s.split_ratio);
        assert_eq!(t.node(p).split_ratio, s.split_ratio);
    }

    #[test]
    fn balance_tree_weighs_ratio_by_leaf_count() {
        // bspwm: src/tree.c balance_tree(): a subtree with 1 leaf against
        // one with 2 leaves gets a 1/3, 2/3 split.
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let _c = insert_client(&mut t, &s, Some(b), 3);
        let root = t.root.unwrap();

        let total = t.balance_tree(Some(root));

        assert_eq!(total, 3);
        assert!((t.node(root).split_ratio - (1.0 / 3.0)).abs() < 1e-9);
    }

    // ---- adjust_ratios ------------------------------------------------

    #[test]
    fn adjust_ratios_preserves_the_fence_pixel_position() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let _b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        t.node_mut(root).split_type = SplitType::Vertical;
        t.node_mut(root).split_ratio = 0.5;
        t.node_mut(root).rect = Rect::new(0, 0, 200, 100);

        // The window grows on the left by 100px; the fence, previously at
        // x=100 (0 + 0.5*200), should end up at ratio 200/300 to stay put.
        t.adjust_ratios(Some(root), Rect::new(-100, 0, 300, 100));

        assert!((t.node(root).split_ratio - (200.0 / 300.0)).abs() < 1e-9);
    }

    // ---- swap_nodes -----------------------------------------------------

    #[test]
    fn swap_nodes_exchanges_tree_position_but_keeps_node_identity() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let c = insert_client(&mut t, &s, Some(b), 3);
        let root = t.root.unwrap();

        assert!(t.swap_nodes(a, c));

        // `a` now sits where `c` used to (child of the b/c split, sibling
        // of `b`), and `c` sits at the old root's first_child slot.
        assert_eq!(t.node(root).first_child(), Some(c));
        let inner = t.node(root).second_child().unwrap();
        assert_eq!(t.brother(a), Some(b));
        assert_eq!(t.node(inner).first_child(), Some(b));
        assert_eq!(t.node(inner).second_child(), Some(a));
        // Node identity (and so the client each id refers to) is
        // unchanged: `a` is still window 1.
        assert_eq!(window_of(&t, a), 1);
        assert_eq!(window_of(&t, c), 3);
    }

    #[test]
    fn swap_nodes_rejects_ancestor_descendant_pairs() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let _b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();

        assert!(!t.swap_nodes(root, a), "a is a descendant of root");
        assert!(!t.swap_nodes(a, a), "a node cannot swap with itself");
    }

    #[test]
    fn swap_nodes_does_not_disturb_focus_by_identity() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let _c = insert_client(&mut t, &s, Some(b), 3);
        t.focus = Some(a);

        t.swap_nodes(a, b);

        assert_eq!(
            t.focus,
            Some(a),
            "focus tracks the node id, not tree position"
        );
    }

    // ---- transplant (transplant_within / transplant_to) -------------------

    #[test]
    fn transplant_within_moves_a_node_to_a_new_anchor() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1); // first_child of root
        let b = insert_client(&mut t, &s, Some(a), 2); // second_child of root
        let c = insert_client(&mut t, &s, Some(b), 3); // splits b: root -> {a, {b, c}}

        // Move `a` to sit next to `c`. `a`'s sibling in the tree is the
        // whole {b, c} split, which takes over the root slot `a` vacates.
        assert!(t.transplant_within(&s, a, Some(c)));

        let root = t.root.unwrap();
        assert_eq!(
            t.node(root).first_child(),
            Some(b),
            "the b/c split is now the root"
        );
        let c_split = t.node(c).parent().unwrap();
        assert_eq!(t.brother(a), Some(c));
        assert_eq!(t.node(c_split).first_child(), Some(c));
        assert_eq!(t.node(c_split).second_child(), Some(a));
    }

    #[test]
    fn transplant_across_two_trees_moves_the_node() {
        let s = settings();
        let mut src = Tree::new();
        let a = insert_client(&mut src, &s, None, 1);
        let b = insert_client(&mut src, &s, Some(a), 2);

        let mut dst = Tree::new();
        let c = insert_client(&mut dst, &s, None, 3);

        let moved = src.transplant_to(&s, b, &mut dst, Some(c));

        assert_eq!(
            src.root,
            Some(a),
            "b's old sibling a now solely occupies src"
        );
        assert!(!src.contains(b), "b's old id is freed in src");
        assert!(dst.contains(moved));
        assert_ne!(
            dst.root,
            Some(c),
            "c was split to make room for the moved node"
        );
        assert_eq!(window_of(&dst, moved), 2);
    }

    #[test]
    fn transplant_rejects_moving_a_node_under_its_own_descendant() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let root = t.root.unwrap();

        assert!(!t.transplant_within(&s, root, Some(a)));
        assert_eq!(
            t.root,
            Some(root),
            "the tree is untouched by a rejected transplant"
        );
    }

    // ---- circulate_leaves -------------------------------------------------

    #[test]
    fn circulate_forward_then_backward_restores_leaf_order() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let c = insert_client(&mut t, &s, Some(b), 3);
        let root = t.root;

        let order_before = leaf_windows(&t, root);
        t.circulate_leaves(&s, root, CirculateDir::Forward);
        let order_after_forward = leaf_windows(&t, root);
        assert_ne!(
            order_before, order_after_forward,
            "forward circulation moves leaves"
        );

        t.circulate_leaves(&s, root, CirculateDir::Backward);
        let order_after_backward = leaf_windows(&t, root);
        assert_eq!(
            order_before, order_after_backward,
            "backward undoes forward"
        );
        let _ = c;
    }

    fn leaf_windows(t: &Tree, root: Option<NodeId>) -> Vec<u32> {
        let mut out = Vec::new();
        let mut f = t.first_extrema(root);
        while let Some(n) = f {
            out.push(window_of(t, n));
            f = t.next_leaf(Some(n), root);
        }
        out
    }

    #[test]
    fn circulate_is_a_no_op_with_fewer_than_two_tiled_leaves() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let root = t.root;
        let before = leaf_windows(&t, root);

        t.circulate_leaves(&s, root, CirculateDir::Forward);

        assert_eq!(before, leaf_windows(&t, root));
        let _ = a;
    }

    // ---- presel -----------------------------------------------------

    #[test]
    fn presel_dir_creates_then_updates_a_pending_presel() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);

        t.presel_dir(a, Direction::North, 0.5);
        assert_eq!(t.node(a).presel.unwrap().split_dir, Direction::North);

        t.presel_dir(a, Direction::South, 0.5);
        assert_eq!(t.node(a).presel.unwrap().split_dir, Direction::South);
    }

    #[test]
    fn presel_ratio_creates_then_updates_a_pending_presel() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);

        t.presel_ratio(a, 0.25, Direction::East);
        assert!((t.node(a).presel.unwrap().split_ratio - 0.25).abs() < 1e-9);

        t.presel_ratio(a, 0.75, Direction::East);
        assert!((t.node(a).presel.unwrap().split_ratio - 0.75).abs() < 1e-9);
    }

    #[test]
    fn cancel_presel_in_clears_the_whole_subtree() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        t.presel_dir(a, Direction::North, 0.5);
        t.presel_dir(b, Direction::South, 0.5);

        t.cancel_presel_in(t.root);

        assert!(t.node(a).presel.is_none());
        assert!(t.node(b).presel.is_none());
    }

    // ---- vacant / hidden propagation --------------------------------

    #[test]
    fn set_vacant_propagates_upward_only_when_both_children_are_vacant() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();

        t.set_vacant(a, true);
        assert!(t.node(a).vacant);
        assert!(!t.node(root).vacant, "b is still occupied");

        t.set_vacant(b, true);
        assert!(t.node(root).vacant, "both children vacant propagates up");
    }

    #[test]
    fn set_hidden_marks_a_tiled_clients_slot_vacant() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let _b = insert_client(&mut t, &s, Some(a), 2);

        t.set_hidden(a, true);

        assert!(t.node(a).hidden);
        assert!(t.node(a).vacant, "hiding a tiled client frees its slot");
    }

    // ---- client state transitions --------------------------------------

    #[test]
    fn set_state_floating_marks_the_tiled_slot_vacant() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let _b = insert_client(&mut t, &s, Some(a), 2);

        assert!(t.set_state(a, ClientState::Floating));

        assert_eq!(
            t.node(a).client.as_ref().unwrap().state,
            ClientState::Floating
        );
        assert!(t.node(a).vacant);
    }

    #[test]
    fn set_state_back_to_tiled_clears_vacant() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let _b = insert_client(&mut t, &s, Some(a), 2);

        t.set_state(a, ClientState::Floating);
        t.set_state(a, ClientState::Tiled);

        assert!(!t.node(a).vacant);
        assert_eq!(
            t.node(a).client.as_ref().unwrap().last_state,
            ClientState::Floating
        );
    }

    #[test]
    fn set_state_no_op_when_already_in_that_state() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        assert!(!t.set_state(a, ClientState::Tiled));
    }

    // ---- apply_layout ---------------------------------------------------

    #[test]
    fn apply_layout_splits_a_vertical_node_by_its_ratio() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        t.node_mut(root).split_type = SplitType::Vertical;
        t.node_mut(root).split_ratio = 0.25;

        let mrect = Rect::new(0, 0, 400, 200);
        t.apply_layout(Some(root), mrect, 0, Layout::Tiled, mrect);

        assert_eq!(t.node(a).rect, Rect::new(0, 0, 100, 200));
        assert_eq!(t.node(b).rect, Rect::new(100, 0, 300, 200));
    }

    #[test]
    fn apply_layout_monocle_gives_every_leaf_the_full_rect() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();

        let mrect = Rect::new(0, 0, 400, 200);
        t.apply_layout(Some(root), mrect, 0, Layout::Monocle, mrect);

        assert_eq!(t.node(a).rect, mrect);
        assert_eq!(t.node(b).rect, mrect);
    }

    #[test]
    fn apply_layout_clamps_the_fence_to_minimum_constraints() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        t.node_mut(root).split_type = SplitType::Vertical;
        t.node_mut(root).split_ratio = 0.05; // would put the fence at 10px
        t.node_mut(a).constraints.min_width = 150;

        let mrect = Rect::new(0, 0, 200, 100);
        t.apply_layout(Some(root), mrect, 0, Layout::Tiled, mrect);

        assert_eq!(t.node(a).rect.width, 150, "clamped up to a's minimum width");
        assert_eq!(t.node(b).rect.width, 50);
        assert!((t.node(root).split_ratio - 0.75).abs() < 1e-9);
    }

    #[test]
    fn apply_layout_fullscreen_uses_the_monitor_rectangle() {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        t.set_state(a, ClientState::Fullscreen);

        let mrect = Rect::new(0, 0, 1920, 1080);
        t.apply_layout(Some(a), Rect::new(6, 6, 100, 100), 6, Layout::Tiled, mrect);

        let tiled_rect = t.node(a).client.as_ref().unwrap().tiled_rectangle;
        assert_eq!(tiled_rect, mrect);
    }
}
