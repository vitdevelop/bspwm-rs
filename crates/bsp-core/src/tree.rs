//! The binary space partitioning tree: one arena of nodes per desktop, and
//! every structural operation bspwm performs on it.
//!
//! bspwm: `src/tree.c` and `src/tree.h`. Every public function below names
//! the bspwm function it mirrors. Functions that only exist in bspwm to
//! drive X11 side effects (drawing borders, EWMH, the input focus, the
//! stacking list, `subscribe` reports) are left out: those are
//! `bsp-compositor`'s job once an adapter exists to receive the
//! [`crate::desktop::Desktop`]-level effects this crate will grow in a
//! later. What remains here is the part bspwm itself calls "the
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

/// Which edge(s) of a node's rectangle a drag handle anchors, matching
/// the 8 combinations bspwm's own `get_handle()` ever produces (a
/// single edge or a single corner) — modeled directly as an enum rather
/// than as a `HANDLE_LEFT | HANDLE_TOP`-style bitmask, since nothing
/// here needs any of the other combinations bspwm's `resize_handle_t`
/// could technically represent but never does. Mirrors
/// `bsp_ipc::value::ResizeHandle`'s own shape (the wire format `bspc
/// node --resize` already parses), which is this type's only caller.
///
/// bspwm: `src/types.h` `resize_handle_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeHandle {
    /// The left edge.
    Left,
    /// The top edge.
    Top,
    /// The right edge.
    Right,
    /// The bottom edge.
    Bottom,
    /// The top-left corner.
    TopLeft,
    /// The top-right corner.
    TopRight,
    /// The bottom-right corner.
    BottomRight,
    /// The bottom-left corner.
    BottomLeft,
}

/// What a pointer button (held with `pointer_modifier`) does when
/// pressed on a node and dragged — bspwm's `bspc config pointer_action1`/
/// `pointer_action2`/`pointer_action3`, one per `BUTTONS[]` slot
/// (`docs/bsp-compositor.md`'s Hotkeys and config progress: this is a
/// compositor-local setting, not a `Settings` field, same reasoning as
/// `hotkeys_inline_bspc`).
///
/// bspwm: `src/types.h` `pointer_action_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerAction {
    /// No action.
    None,
    /// Focuses (or, if already focused, raises) the node under the
    /// pointer; never a drag.
    Focus,
    /// Moves the node, following the pointer.
    Move,
    /// Resizes the node from whichever single edge the pointer's
    /// initial position was nearest (`get_handle`).
    ResizeSide,
    /// Resizes the node from whichever corner the pointer's initial
    /// position was nearest.
    ResizeCorner,
}

impl ResizeHandle {
    fn left(self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    fn top(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    fn right(self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    fn bottom(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }
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

/// Where the preselection feedback of a node at `node_rect` is drawn: the
/// part the next window will take, inside the node's rectangle less the
/// window gap. North and West take `split_ratio` of the size, East and South
/// the rest, against the far edge.
///
/// bspwm: `src/window.c` `draw_presel_feedback()`.
pub fn presel_rect(node_rect: Rect, presel: Presel, gap: i32) -> Rect {
    let width = (node_rect.width - gap).max(0);
    let height = (node_rect.height - gap).max(0);
    let ratio = presel.split_ratio;
    let (mut x, mut y, mut w, mut h) = (0, 0, width, height);
    match presel.split_dir {
        Direction::North => h = (ratio * f64::from(height)) as i32,
        Direction::East => {
            w = ((1.0 - ratio) * f64::from(width)) as i32;
            x = width - w;
        }
        Direction::South => {
            h = ((1.0 - ratio) * f64::from(height)) as i32;
            y = height - h;
        }
        Direction::West => w = (ratio * f64::from(width)) as i32,
    }
    Rect::new(node_rect.x + x, node_rect.y + y, w, h)
}

/// What [`Tree::swap_subtrees_with`] moved: for each direction the new root id
/// and every `(old, new)` node id pair.
#[derive(Debug, Clone)]
pub struct SubtreeSwap {
    /// The subtree that left `self` for `other`.
    pub into_other: (NodeId, Vec<(NodeId, NodeId)>),
    /// The subtree that left `other` for `self`.
    pub into_self: (NodeId, Vec<(NodeId, NodeId)>),
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

/// The settings `Tree::apply_layout` reads, gathered so a layout is one call.
///
/// bspwm: the globals `gapless_monocle`, `borderless_monocle`,
/// `borderless_singleton` and `center_pseudo_tiled`, read by `apply_layout()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutOptions {
    /// Drop the window gap in monocle layout.
    pub gapless_monocle: bool,
    /// No border on tiled windows in monocle layout.
    pub borderless_monocle: bool,
    /// No border on the only window: `borderless_singleton` is set and this is
    /// the only monitor (the tree's root must also be a single window).
    pub borderless_singleton: bool,
    /// Centre a pseudo-tiled window in its slot.
    pub center_pseudo_tiled: bool,
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

    /// Every node reachable from the root (splits included), in pre-order.
    pub fn node_ids(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.root.into_iter().collect();
        while let Some(n) = stack.pop() {
            out.push(n);
            let node = self.node(n);
            // Pushed second child first, so the first child is visited first.
            stack.extend(node.second_child);
            stack.extend(node.first_child);
        }
        out
    }

    /// Creates a bare, parentless node (an empty receptacle once inserted).
    ///
    /// bspwm: `src/tree.c` `make_node()`.
    pub fn new_node(&mut self, settings: &Settings) -> NodeId {
        self.alloc(Node::new(settings))
    }

    /// Creates a leaf node already holding `client`.
    pub fn new_client_node(&mut self, settings: &Settings, mut client: Client) -> NodeId {
        let mut node = Node::new(settings);
        // bspwm: `make_client()` takes the setting's value.
        client.honor_size_hints = settings.honor_size_hints;
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

    /// bspwm: `src/tree.c` `next_node()`: the next node in in-order (a node's
    /// first subtree, the node itself, its second subtree), internal nodes
    /// included; `None` after the last.
    pub fn next_node(&self, id: Option<NodeId>) -> Option<NodeId> {
        let n = id?;
        if let Some(second) = self.node(n).second_child {
            return self.first_extrema(Some(second));
        }
        let mut p = n;
        while self.is_second_child(p) {
            p = self.node(p).parent?;
        }
        if self.is_first_child(p) {
            self.node(p).parent
        } else {
            None
        }
    }

    /// bspwm: `src/tree.c` `prev_node()`: the reverse of [`Tree::next_node`].
    pub fn prev_node(&self, id: Option<NodeId>) -> Option<NodeId> {
        let n = id?;
        if let Some(first) = self.node(n).first_child {
            return self.second_extrema(Some(first));
        }
        let mut p = n;
        while self.is_first_child(p) {
            p = self.node(p).parent?;
        }
        if self.is_second_child(p) {
            self.node(p).parent
        } else {
            None
        }
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
    pub fn get_rectangle(&self, id: NodeId, window_gap: i32, layout: Layout, gapless_monocle: bool) -> Rect {
        let node = self.node(id);
        if let Some(c) = &node.client {
            return if c.state == ClientState::Floating {
                c.floating_rectangle
            } else {
                c.tiled_rectangle
            };
        }
        let wg = if gapless_monocle && layout == Layout::Monocle {
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

    /// The two children of a split node; `None` for a leaf (or a corrupt split
    /// that lacks one), where the callers just stop instead of panicking.
    fn children(&self, id: NodeId) -> Option<(NodeId, NodeId)> {
        let n = self.node(id);
        Some((n.first_child?, n.second_child?))
    }

    fn propagate_vacant_upward(&mut self, id: Option<NodeId>) {
        let Some(id) = id else { return };
        let parent = self.node(id).parent;
        if let Some(p) = parent {
            let both_vacant = self.children(p).is_some_and(|(a, b)| self.node(a).vacant && self.node(b).vacant);
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
            let both_hidden = self.children(p).is_some_and(|(a, b)| self.node(a).hidden && self.node(b).hidden);
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
        let Some((first, second)) = self.children(id) else {
            debug_assert!(false, "a split node without two children");
            return;
        };
        let split_type = self.node(id).split_type;
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
            let Some((fc, sc)) = self.children(p) else {
                debug_assert!(false, "a split node without two children");
                return;
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
            if let Some(c) = self.node_mut(id).client.as_mut() {
                c.last_state = last_state;
                c.state = s;
            }
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
            // bspwm starts both best areas at 0 and compares with `>`, so a
            // leaf with no area is never picked.
            if !node.vacant && self.node_area(n) > 0 {
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

    /// Where [`Tree::insert_node`] puts a node anchored at `f` when `f` is
    /// private (or under a private node) and has no preselection: at a public
    /// leaf instead, if there is one; and if that is still private, split
    /// along the anchor's longer side, a preselection `insert_node` makes and
    /// then uses up. Returns the anchor and that preselection.
    ///
    /// bspwm: `src/tree.c` `insert_node()`'s private branch.
    pub fn private_insertion(&self, f: NodeId) -> (NodeId, Option<Direction>) {
        let privately = |t: &Self, f: NodeId| {
            t.node(f).presel.is_none() && (t.node(f).private || t.node(f).parent.is_some_and(|p| t.private_count(Some(p)) > 0))
        };
        if !privately(self, f) {
            return (f, None);
        }
        let f = self.find_public(self.root).unwrap_or(f);
        if !privately(self, f) {
            return (f, None);
        }
        let rect = self.node(f).rect;
        (f, Some(if rect.width >= rect.height { Direction::East } else { Direction::South }))
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
            Some(f_id) => {
                let (f_id, presel) = self.private_insertion(f_id);
                let p = self.node(f_id).parent;
                if let Some(dir) = presel {
                    self.presel_dir(f_id, dir, settings.split_ratio);
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
                                let Some((fc, sc)) = self.children(q_id) else { break };
                                if self.node(fc).vacant || self.node(sc).vacant {
                                    q = self.node(q_id).parent;
                                } else {
                                    break;
                                }
                            }
                            if q.or(p).is_some_and(|q| self.node(q).split_type == SplitType::Horizontal) {
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
                            // Not reachable: the condition above holds for `None`.
                            debug_assert!(false, "spiral insertion without a parent");
                            self.free_slot(c);
                            self.node_mut(n).parent = None;
                            return None;
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
                    let Some(presel) = self.node(f_id).presel else {
                        debug_assert!(false, "the presel branch without a presel");
                        self.free_slot(c);
                        self.node_mut(n).parent = None;
                        return None;
                    };
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

        let Some(b) = self.brother(n) else {
            // A split always has two children; without one, just detach `n`.
            debug_assert!(false, "a split node without two children");
            self.node_mut(n).parent = None;
            return;
        };
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

    // ---- Move and resize --------------------------------------------

    /// Finds the nearest ancestor of `id` whose split forms the edge a
    /// drag toward `dir` would grab: the first ancestor `p` whose split
    /// axis matches `dir` and whose own rectangle extends past `id`'s on
    /// that side. Every ancestor is compared against `id`'s own
    /// rectangle throughout the walk up (not a running one updated at
    /// each step) — bspwm keeps this exact comparison, so a `dir` that
    /// never actually bounds `id` (e.g. `id` is already flush against
    /// the desktop's own edge on that side) correctly finds no fence at
    /// all, rather than the nearest split regardless of which side it's
    /// on.
    ///
    /// bspwm: `src/tree.c` `find_fence()`.
    #[must_use]
    pub fn find_fence(&self, id: NodeId, dir: Direction) -> Option<NodeId> {
        let rect = self.node(id).rect;
        let mut p = self.node(id).parent();
        while let Some(pid) = p {
            let pn = self.node(pid);
            let bounds = match dir {
                Direction::North => pn.split_type == SplitType::Horizontal && pn.rect.y < rect.y,
                Direction::West => pn.split_type == SplitType::Vertical && pn.rect.x < rect.x,
                Direction::South => {
                    pn.split_type == SplitType::Horizontal && pn.rect.bottom() > rect.bottom()
                }
                Direction::East => {
                    pn.split_type == SplitType::Vertical && pn.rect.right() > rect.right()
                }
            };
            if bounds {
                return Some(pid);
            }
            p = pn.parent();
        }
        None
    }

    /// `id`'s current on-screen rectangle regardless of state:
    /// `floating_rectangle` for `Floating`, `tiled_rectangle` for
    /// everything else (`Tiled`/`PseudoTiled`/`Fullscreen`).
    ///
    /// bspwm: `src/tree.c` `get_rectangle()`'s client branch (`IS_FLOATING`).
    fn client_rect(&self, id: NodeId) -> Option<Rect> {
        let client = self.node(id).client.as_ref()?;
        Some(if client.state == ClientState::Floating {
            client.floating_rectangle
        } else {
            client.tiled_rectangle
        })
    }

    /// Which edge or corner of `id`'s current rectangle a click at
    /// `pos` (in the same coordinate space as node rectangles) is
    /// closest to, for `action`. `ResizeSide` splits the rectangle
    /// along both diagonals and picks whichever of the four triangles
    /// `pos` falls in; `ResizeCorner` (and, matching bspwm, every other
    /// `action` too — the result is simply unused for `Move`/`Focus`/
    /// `None`) picks whichever quadrant of the midpoint `pos` falls in.
    /// Falls back to `BottomRight` if `id` names no client (bspwm:
    /// `get_handle()`'s own `rh = HANDLE_BOTTOM_RIGHT` default, though
    /// that path is never actually reachable there either).
    ///
    /// bspwm: `src/pointer.c` `get_handle()`.
    #[must_use]
    pub fn get_handle(&self, id: NodeId, pos: (i32, i32), action: PointerAction) -> ResizeHandle {
        let Some(rect) = self.client_rect(id) else {
            return ResizeHandle::BottomRight;
        };
        if action == PointerAction::ResizeSide {
            let w = f64::from(rect.width);
            let h = f64::from(rect.height);
            let ratio = w / h;
            let x = f64::from(pos.0 - rect.x);
            let y = f64::from(pos.1 - rect.y);
            let diag_a = ratio * y;
            let diag_b = w - diag_a;
            return if x < diag_a {
                if x < diag_b {
                    ResizeHandle::Left
                } else {
                    ResizeHandle::Bottom
                }
            } else if x < diag_b {
                ResizeHandle::Top
            } else {
                ResizeHandle::Right
            };
        }
        let mid_x = rect.x + rect.width / 2;
        let mid_y = rect.y + rect.height / 2;
        match (pos.0 > mid_x, pos.1 > mid_y) {
            (true, true) => ResizeHandle::BottomRight,
            (true, false) => ResizeHandle::TopRight,
            (false, true) => ResizeHandle::BottomLeft,
            (false, false) => ResizeHandle::TopLeft,
        }
    }

    /// Translates `id`'s `floating_rectangle` by `(dx, dy)`. Fails
    /// (`false`) for a `Tiled`/`PseudoTiled` node: bspwm's own
    /// `move_client()` only takes that node down this path while a
    /// pointer drag is actively being tracked (its `grabbing` global),
    /// which a `bspc node --move` request never is — `src/messages.c`
    /// `cmd_node()` calls `move_client()` directly, bypassing
    /// `grab_pointer()`/`track_pointer()` (and so `grabbing`) entirely.
    /// This build has no live-pointer-drag caller yet (`docs/design.md`
    /// roadmap), so the condition simplifies to "always fails for a
    /// tiled node" without losing any real, reachable behavior.
    /// `Fullscreen` has no dedicated guard in bspwm either — a real, if
    /// surprising, quirk kept here rather than silently "fixed": its
    /// `floating_rectangle` moves invisibly, only becoming apparent once
    /// the node later stops being fullscreen.
    ///
    /// Does not transfer `id` to a different monitor if the translated
    /// rectangle would now sit under one (bspwm: `move_client()`'s
    /// `monitor_from_client`/`transfer_node` tail) — that needs a
    /// `Wm`-level, cross-tree operation this `Tree`-scoped function has
    /// no way to perform; deferred alongside `docs/bsp-ipc.md`'s
    /// existing cross-desktop/monitor `node --swap` gap.
    ///
    /// bspwm: `src/window.c` `move_client()`.
    #[must_use]
    pub fn move_floating(&mut self, id: NodeId, dx: i32, dy: i32) -> bool {
        let Some(client) = self.node(id).client.as_ref() else {
            return false;
        };
        if client.state.is_tiled() {
            return false;
        }
        let Some(client) = self.node_mut(id).client.as_mut() else {
            return false;
        };
        client.floating_rectangle.x += dx;
        client.floating_rectangle.y += dy;
        true
    }

    /// Resizes `id` by dragging `handle`. `relative` (always `true` for
    /// `bspc node --resize`, `src/messages.c` `cmd_node()`'s own
    /// `resize_client(&trg, rh, dx, dy, true)` call) treats `dx`/`dy` as
    /// pixel deltas; `!relative` (bspwm: only ever used from a live
    /// pointer drag honoring ICCCM size hints, `src/pointer.c`
    /// `track_pointer()` — this build tracks no such hints yet,
    /// `crate::node`'s module doc comment) treats them as an absolute
    /// position along the dragged edge(s) instead.
    ///
    /// `Fullscreen` never resizes. `Tiled` adjusts one or two ancestor
    /// "fence" nodes' `split_ratio` (`find_fence`, `adjust_ratios`)
    /// instead of `id`'s own rectangle — the caller must still
    /// re-arrange the desktop afterward for this to take visible effect
    /// (`apply_layout` recomputes `id`'s `rect` from its ancestors'
    /// ratios; this only touches the tree). Every other state
    /// (`Floating`, `PseudoTiled`) adjusts `floating_rectangle` directly
    /// from the node's *current on-screen* rectangle (`tiled_rectangle`
    /// for `PseudoTiled`, matching bspwm's own `get_rectangle()`, not
    /// its possibly-larger stored preference), clamped to a 1×1 minimum
    /// — for `PseudoTiled` this only changes the *preferred* size
    /// `apply_layout` reads back, so the caller must still re-arrange
    /// for that state too, same as `Tiled`.
    ///
    /// bspwm: `src/window.c` `resize_client()`.
    #[must_use]
    pub fn resize_node(
        &mut self,
        id: NodeId,
        handle: ResizeHandle,
        dx: i32,
        dy: i32,
        relative: bool,
    ) -> bool {
        let Some(client) = self.node(id).client.clone() else {
            return false;
        };
        if client.state == ClientState::Fullscreen {
            return false;
        }

        if client.state == ClientState::Tiled {
            let vertical_fence = if handle.left() {
                self.find_fence(id, Direction::West)
            } else if handle.right() {
                self.find_fence(id, Direction::East)
            } else {
                None
            };
            let horizontal_fence = if handle.top() {
                self.find_fence(id, Direction::North)
            } else if handle.bottom() {
                self.find_fence(id, Direction::South)
            } else {
                None
            };
            if vertical_fence.is_none() && horizontal_fence.is_none() {
                return false;
            }
            if let Some(f) = vertical_fence {
                let rect = self.node(f).rect;
                let sr = if relative {
                    self.node(f).split_ratio + dx as f64 / rect.width as f64
                } else {
                    (dx - rect.x) as f64 / rect.width as f64
                };
                self.node_mut(f).split_ratio = sr.clamp(0.0, 1.0);
                self.adjust_ratios(Some(f), rect);
            }
            if let Some(f) = horizontal_fence {
                let rect = self.node(f).rect;
                let sr = if relative {
                    self.node(f).split_ratio + dy as f64 / rect.height as f64
                } else {
                    (dy - rect.y) as f64 / rect.height as f64
                };
                self.node_mut(f).split_ratio = sr.clamp(0.0, 1.0);
                self.adjust_ratios(Some(f), rect);
            }
            return true;
        }

        let Some(rect) = self.client_rect(id) else {
            return false;
        };
        let mut width = rect.width;
        let mut height = rect.height;
        if relative {
            width += dx
                * if handle.left() {
                    -1
                } else if handle.right() {
                    1
                } else {
                    0
                };
            height += dy
                * if handle.top() {
                    -1
                } else if handle.bottom() {
                    1
                } else {
                    0
                };
        } else {
            if handle.left() {
                width = rect.x + rect.width - dx;
            } else if handle.right() {
                width = dx - rect.x;
            }
            if handle.top() {
                height = rect.y + rect.height - dy;
            } else if handle.bottom() {
                height = dy - rect.y;
            }
        }
        width = width.max(1);
        height = height.max(1);
        (width, height) = client.apply_size_hints(width, height);
        let mut x = rect.x;
        let mut y = rect.y;
        if handle.left() {
            x += rect.width - width;
        }
        if handle.top() {
            y += rect.height - height;
        }
        let Some(client) = self.node_mut(id).client.as_mut() else {
            return false;
        };
        client.floating_rectangle = Rect::new(x, y, width, height);
        true
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

    /// Exchanges the subtree at `n1` of this tree with the subtree at `n2` of
    /// `other`: each lands in the exact slot (same parent, same side, or as the
    /// root) the other left. Returns, per side, `(new root id, (old, new) pairs)`:
    /// first the subtree that went into `other`, then the one that came here.
    /// A tree whose focus left gets the incoming subtree's root as its focus,
    /// or, when both focuses moved, the focus that came with it.
    ///
    /// bspwm: `src/tree.c` `swap_nodes()`, the branch for two different desktops.
    pub fn swap_subtrees_with(&mut self, n1: NodeId, other: &mut Tree, n2: NodeId) -> SubtreeSwap {
        let slot1 = (self.node(n1).parent, self.is_first_child(n1));
        let slot2 = (other.node(n2).parent, other.is_first_child(n2));
        let focus1_left = self.is_descendant(self.focus, Some(n1));
        let focus2_left = other.is_descendant(other.focus, Some(n2));
        let (focus1, focus2) = (self.focus, other.focus);

        let mut into_other = Vec::new();
        let mut into_self = Vec::new();
        let new1 = self.clone_subtree_into(n1, other, &mut into_other);
        let new2 = other.clone_subtree_into(n2, self, &mut into_self);

        let place = |tree: &mut Tree, new: NodeId, (parent, first): (Option<NodeId>, bool)| {
            tree.node_mut(new).parent = parent;
            match parent {
                Some(p) if first => tree.node_mut(p).first_child = Some(new),
                Some(p) => tree.node_mut(p).second_child = Some(new),
                None => tree.root = Some(new),
            }
        };
        place(self, new2, slot1);
        place(other, new1, slot2);
        self.free_node(n1);
        other.free_node(n2);

        // bspwm: `d1->focus = n2_held_focus ? last_d2_focus : n2`, and the same
        // for `d2`: the incoming subtree's root, or the focus that came with it.
        let map = |pairs: &[(NodeId, NodeId)], old: Option<NodeId>| old.and_then(|o| pairs.iter().find(|(from, _)| *from == o).map(|(_, to)| *to));
        if focus1_left {
            self.focus = Some(if focus2_left { map(&into_self, focus2).unwrap_or(new2) } else { new2 });
        }
        if focus2_left {
            other.focus = Some(if focus1_left { map(&into_other, focus1).unwrap_or(new1) } else { new1 });
        }
        for (tree, node) in [(&mut *self, new2), (&mut *other, new1)] {
            tree.propagate_flags_upward(Some(node));
            tree.rebuild_constraints_towards_root(Some(node));
        }
        SubtreeSwap { into_other: (new1, into_other), into_self: (new2, into_self) }
    }

    /// Clones the subtree rooted at `id` into `dest`'s arena (fresh
    /// `NodeId`s throughout: a `NodeId` is only ever meaningful within the
    /// `Tree` that issued it, see `crate::id`), preserving every field and
    /// the parent/child structure, and returns the new root's id in
    /// `dest`. `self` is left unchanged; callers that are moving rather
    /// than copying still need to unlink and free the original.
    fn clone_subtree_into(&self, id: NodeId, dest: &mut Tree, moved: &mut Vec<(NodeId, NodeId)>) -> NodeId {
        let node = self.node(id);
        let (first, second) = (node.first_child, node.second_child);
        let mut copy = node.clone();
        copy.parent = None;
        copy.first_child = None;
        copy.second_child = None;
        let new_id = dest.alloc(copy);
        moved.push((id, new_id));

        if let Some(f) = first {
            let new_f = self.clone_subtree_into(f, dest, moved);
            dest.node_mut(new_f).parent = Some(new_id);
            dest.node_mut(new_id).first_child = Some(new_f);
        }
        if let Some(s) = second {
            let new_s = self.clone_subtree_into(s, dest, moved);
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
        self.transplant_to_mapped(settings, n, dest, anchor).0
    }

    /// As [`Tree::transplant_to`], also returning every `(old, new)` node id
    /// pair of the moved subtree (the root first), so a caller that keeps ids
    /// of its own (`bsp-ipc`'s registry) can carry each of them over.
    pub fn transplant_to_mapped(
        &mut self,
        settings: &Settings,
        n: NodeId,
        dest: &mut Tree,
        anchor: Option<NodeId>,
    ) -> (NodeId, Vec<(NodeId, NodeId)>) {
        self.unlink_node(settings, n);
        let mut moved = Vec::new();
        let new_id = self.clone_subtree_into(n, dest, &mut moved);
        self.free_node(n);
        dest.insert_node(settings, new_id, anchor);
        (new_id, moved)
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
                while let (Some(f_id), Some(s_id)) = (f, s) {
                    self.swap_nodes(f_id, s_id);
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
                while let (Some(f_id), Some(s_id)) = (f, s) {
                    self.swap_nodes(f_id, s_id);
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
        options: LayoutOptions,
    ) {
        let Some(id) = id else { return };
        self.node_mut(id).rect = rect;

        if self.is_leaf(id) {
            let Some(client) = self.node(id).client.clone() else {
                return;
            };

            // bspwm: `bw = 0` for a fullscreen window, a tiled window in monocle
            // with `borderless_monocle`, and the only window with
            // `borderless_singleton`.
            let the_only_window = options.borderless_singleton
                && self.root.is_some_and(|r| self.node(r).client.is_some());
            let bw = if (options.borderless_monocle && layout == Layout::Monocle && client.state.is_tiled())
                || the_only_window
                || client.state == ClientState::Fullscreen
            {
                0
            } else {
                client.border_width
            };
            let r = match client.state {
                ClientState::Tiled | ClientState::PseudoTiled => {
                    let wg = if options.gapless_monocle && layout == Layout::Monocle {
                        0
                    } else {
                        window_gap
                    };
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
                        if options.center_pseudo_tiled {
                            r.x = rect.x - bw + (rect.width - wg - r.width) / 2;
                            r.y = rect.y - bw + (rect.height - wg - r.height) / 2;
                        }
                    }
                    r
                }
                ClientState::Floating => client.floating_rectangle,
                ClientState::Fullscreen => monitor_rect,
            };

            if let Some(c) = &mut self.node_mut(id).client {
                c.shown_border_width = bw;
            }
            // bspwm's `apply_layout()` leaves a floating client's
            // `tiled_rectangle` alone: it keeps the slot it last had while tiled.
            if client.state != ClientState::Floating {
                if let Some(c) = &mut self.node_mut(id).client {
                    c.tiled_rectangle = r;
                }
            }
            return;
        }

        let (first, second, split_type, split_ratio, first_vacant, second_vacant) = {
            let Some((first, second)) = self.children(id) else {
                debug_assert!(false, "a split node without two children");
                return;
            };
            let n = self.node(id);
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

        self.apply_layout(Some(first), first_rect, window_gap, layout, monitor_rect, options);
        self.apply_layout(Some(second), second_rect, window_gap, layout, monitor_rect, options);
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

    // ---- find_fence / resize_node / move_floating ------------------------

    /// A two-leaf vertical split (`a` | `b`) laid out over `0,0 400x200`,
    /// ready for `find_fence`/`resize_node` tests.
    fn vertical_split_fixture() -> (Tree, NodeId, NodeId) {
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let root = t.root.unwrap();
        t.node_mut(root).split_type = SplitType::Vertical;
        t.node_mut(root).split_ratio = 0.5;
        let mrect = Rect::new(0, 0, 400, 200);
        t.apply_layout(Some(root), mrect, 0, Layout::Tiled, mrect, LayoutOptions::default());
        (t, a, b)
    }

    #[test]
    fn find_fence_finds_the_shared_ancestor_on_the_bounding_side_only() {
        let (t, a, b) = vertical_split_fixture();
        let root = t.root.unwrap();
        // `a` is the left leaf: its east side (and `b`'s west side) is
        // the shared fence; neither leaf is bounded on the other two
        // sides (they're flush against the desktop edge there).
        assert_eq!(t.find_fence(a, Direction::East), Some(root));
        assert_eq!(t.find_fence(b, Direction::West), Some(root));
        assert_eq!(t.find_fence(a, Direction::West), None);
        assert_eq!(t.find_fence(b, Direction::East), None);
        assert_eq!(t.find_fence(a, Direction::North), None);
        assert_eq!(t.find_fence(a, Direction::South), None);
    }

    #[test]
    fn resize_node_tiled_adjusts_the_fence_split_ratio() {
        let (mut t, _a, b) = vertical_split_fixture();
        let root = t.root.unwrap();
        // Dragging `b`'s left (west) edge 40px to the left grows `b` and
        // shrinks `a`, moving the fence from x=200 (ratio 0.5) to x=160.
        assert!(t.resize_node(b, ResizeHandle::Left, -40, 0, true));
        assert!((t.node(root).split_ratio - 0.4).abs() < 1e-9);
    }

    #[test]
    fn resize_node_tiled_fails_when_the_handle_names_no_fence() {
        let (mut t, a, _b) = vertical_split_fixture();
        // `a` is the leftmost leaf: nothing bounds its own left edge.
        assert!(!t.resize_node(a, ResizeHandle::Left, -40, 0, true));
    }

    #[test]
    fn resize_node_floating_grows_from_the_dragged_corner_and_clamps_to_one_pixel() {
        let s = settings();
        let mut t = Tree::new();
        let n = insert_client(&mut t, &s, None, 1);
        {
            let c = t.node_mut(n).client.as_mut().unwrap();
            c.state = ClientState::Floating;
            c.floating_rectangle = Rect::new(0, 0, 100, 100);
        }

        assert!(t.resize_node(n, ResizeHandle::BottomRight, 20, 20, true));
        assert_eq!(
            t.node(n).client.as_ref().unwrap().floating_rectangle,
            Rect::new(0, 0, 120, 120)
        );

        // Dragging the right edge far enough left to cross the left
        // edge clamps to a 1px-wide rectangle rather than going
        // negative.
        assert!(t.resize_node(n, ResizeHandle::Right, -1000, 0, true));
        let r = t.node(n).client.as_ref().unwrap().floating_rectangle;
        assert_eq!(r.width, 1);
    }

    #[test]
    fn resize_node_fullscreen_never_resizes() {
        let s = settings();
        let mut t = Tree::new();
        let n = insert_client(&mut t, &s, None, 1);
        t.node_mut(n).client.as_mut().unwrap().state = ClientState::Fullscreen;
        assert!(!t.resize_node(n, ResizeHandle::BottomRight, 20, 20, true));
    }

    #[test]
    fn move_floating_translates_the_floating_rectangle() {
        let s = settings();
        let mut t = Tree::new();
        let n = insert_client(&mut t, &s, None, 1);
        {
            let c = t.node_mut(n).client.as_mut().unwrap();
            c.state = ClientState::Floating;
            c.floating_rectangle = Rect::new(10, 10, 50, 50);
        }
        assert!(t.move_floating(n, 5, -5));
        assert_eq!(
            t.node(n).client.as_ref().unwrap().floating_rectangle,
            Rect::new(15, 5, 50, 50)
        );
    }

    #[test]
    fn move_floating_fails_for_a_tiled_node() {
        // bspwm: `move_client()` only moves a tiled node while a pointer
        // drag is actively being tracked — never reachable from `bspc
        // node --move`.
        let (mut t, a, _b) = vertical_split_fixture();
        assert!(!t.move_floating(a, 5, 5));
    }

    #[test]
    fn get_handle_resize_corner_picks_the_nearest_quadrant() {
        let s = settings();
        let mut t = Tree::new();
        let n = insert_client(&mut t, &s, None, 1);
        {
            let c = t.node_mut(n).client.as_mut().unwrap();
            c.state = ClientState::Floating;
            c.floating_rectangle = Rect::new(0, 0, 100, 100);
        }
        assert_eq!(
            t.get_handle(n, (80, 20), PointerAction::ResizeCorner),
            ResizeHandle::TopRight
        );
        assert_eq!(
            t.get_handle(n, (20, 80), PointerAction::ResizeCorner),
            ResizeHandle::BottomLeft
        );
        assert_eq!(
            t.get_handle(n, (20, 20), PointerAction::ResizeCorner),
            ResizeHandle::TopLeft
        );
        assert_eq!(
            t.get_handle(n, (80, 80), PointerAction::ResizeCorner),
            ResizeHandle::BottomRight
        );
    }

    #[test]
    fn get_handle_resize_side_picks_the_nearest_edge() {
        let s = settings();
        let mut t = Tree::new();
        let n = insert_client(&mut t, &s, None, 1);
        {
            let c = t.node_mut(n).client.as_mut().unwrap();
            c.state = ClientState::Floating;
            c.floating_rectangle = Rect::new(0, 0, 100, 100);
        }
        // A square window: each edge "owns" the triangle nearest it,
        // split by both diagonals.
        assert_eq!(
            t.get_handle(n, (50, 5), PointerAction::ResizeSide),
            ResizeHandle::Top
        );
        assert_eq!(
            t.get_handle(n, (50, 95), PointerAction::ResizeSide),
            ResizeHandle::Bottom
        );
        assert_eq!(
            t.get_handle(n, (5, 50), PointerAction::ResizeSide),
            ResizeHandle::Left
        );
        assert_eq!(
            t.get_handle(n, (95, 50), PointerAction::ResizeSide),
            ResizeHandle::Right
        );
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
    fn presel_rect_follows_draw_presel_feedback() {
        let r = Rect::new(100, 50, 406, 206);
        let p = |split_dir, split_ratio| Presel { split_ratio, split_dir };
        // A 400x200 area once the 6px gap is taken off.
        assert_eq!(presel_rect(r, p(Direction::East, 0.3), 6), Rect::new(100 + 120, 50, 280, 200));
        assert_eq!(presel_rect(r, p(Direction::West, 0.3), 6), Rect::new(100, 50, 120, 200));
        assert_eq!(presel_rect(r, p(Direction::North, 0.25), 6), Rect::new(100, 50, 400, 50));
        assert_eq!(presel_rect(r, p(Direction::South, 0.25), 6), Rect::new(100, 50 + 50, 400, 150));
    }

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
        t.apply_layout(Some(root), mrect, 0, Layout::Tiled, mrect, LayoutOptions::default());

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
        t.apply_layout(Some(root), mrect, 0, Layout::Monocle, mrect, LayoutOptions::default());

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
        t.apply_layout(Some(root), mrect, 0, Layout::Tiled, mrect, LayoutOptions::default());

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
        t.apply_layout(Some(a), Rect::new(6, 6, 100, 100), 6, Layout::Tiled, mrect, LayoutOptions::default());

        let tiled_rect = t.node(a).client.as_ref().unwrap().tiled_rectangle;
        assert_eq!(tiled_rect, mrect);
    }

    #[test]
    fn monocle_keeps_the_window_gap_unless_gapless_monocle_is_set() {
        // bspwm: `apply_layout()`'s `wg = gapless_monocle && MONOCLE ? 0 : window_gap`.
        let s = settings();
        let mrect = Rect::new(0, 0, 400, 200);
        for (gapless, want_width) in [(false, 400 - 6 - 2), (true, 400 - 2)] {
            let mut t = Tree::new();
            let a = insert_client(&mut t, &s, None, 1);
            t.apply_layout(Some(a), mrect, 6, Layout::Monocle, mrect, LayoutOptions { gapless_monocle: gapless, ..Default::default() });
            let got = t.node(a).client.as_ref().unwrap().tiled_rectangle.width;
            assert_eq!(got, want_width, "gapless_monocle = {gapless}");
            assert_eq!(t.get_rectangle(a, 6, Layout::Monocle, gapless).width, want_width);
        }
        // A client-less node is measured the same way.
        let mut t = Tree::new();
        let r = t.new_node(&s);
        t.insert_node(&s, r, None);
        t.apply_layout(Some(r), mrect, 6, Layout::Monocle, mrect, LayoutOptions::default());
        assert_eq!(t.get_rectangle(r, 6, Layout::Monocle, false).width, 394);
        assert_eq!(t.get_rectangle(r, 6, Layout::Monocle, true).width, 400);
    }

    #[test]
    fn a_floating_client_keeps_the_tiled_rectangle_it_last_had() {
        // bspwm: `apply_layout()` only writes `tiled_rectangle` for tiled,
        // pseudo-tiled and fullscreen clients.
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let mrect = Rect::new(0, 0, 400, 200);
        t.apply_layout(Some(a), mrect, 0, Layout::Tiled, mrect, LayoutOptions::default());
        let slot = t.node(a).client.as_ref().unwrap().tiled_rectangle;
        t.node_mut(a).client.as_mut().unwrap().floating_rectangle = Rect::new(10, 10, 50, 50);
        t.set_state(a, ClientState::Floating);
        t.apply_layout(Some(a), mrect, 0, Layout::Tiled, mrect, LayoutOptions::default());
        assert_eq!(t.node(a).client.as_ref().unwrap().tiled_rectangle, slot);
    }

    #[test]
    fn find_public_ignores_leaves_with_no_area() {
        // bspwm: `find_public()` starts both best areas at 0 and compares with `>`.
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        assert_eq!(t.find_public(t.root), None, "a leaf never laid out has no area");
        let mrect = Rect::new(0, 0, 400, 200);
        t.apply_layout(Some(a), mrect, 0, Layout::Tiled, mrect, LayoutOptions::default());
        assert_eq!(t.find_public(t.root), Some(a));
    }

    #[test]
    fn borders_are_dropped_where_bspwm_drops_them() {
        // bspwm: `apply_layout()`'s `bw = 0` cases.
        let s = settings();
        let mrect = Rect::new(0, 0, 400, 200);
        let shown = |t: &Tree, n: NodeId| t.node(n).client.as_ref().unwrap().shown_border_width;

        // borderless_monocle: tiled windows in monocle only.
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let borderless = LayoutOptions { borderless_monocle: true, ..Default::default() };
        t.apply_layout(t.root, mrect, 0, Layout::Monocle, mrect, borderless);
        assert_eq!((shown(&t, a), shown(&t, b)), (0, 0));
        assert_eq!(t.node(a).client.as_ref().unwrap().tiled_rectangle.width, 400, "the border's room goes to the window");
        t.apply_layout(t.root, mrect, 0, Layout::Tiled, mrect, borderless);
        assert_eq!(shown(&t, a), 1);

        // borderless_singleton: a lone window on the only monitor, not two.
        let singleton = LayoutOptions { borderless_singleton: true, ..Default::default() };
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        t.apply_layout(t.root, mrect, 0, Layout::Tiled, mrect, singleton);
        assert_eq!(shown(&t, a), 0);
        let b = insert_client(&mut t, &s, Some(a), 2);
        t.apply_layout(t.root, mrect, 0, Layout::Tiled, mrect, singleton);
        assert_eq!((shown(&t, a), shown(&t, b)), (1, 1));

        // A fullscreen window never has one, and the configured width is kept.
        t.set_state(b, ClientState::Fullscreen);
        t.apply_layout(t.root, mrect, 0, Layout::Tiled, mrect, LayoutOptions::default());
        assert_eq!(shown(&t, b), 0);
        assert_eq!(t.node(b).client.as_ref().unwrap().border_width, 1);
    }

    #[test]
    fn a_pseudo_tiled_window_is_centred_in_its_slot_when_asked() {
        // bspwm: `apply_layout()`'s `center_pseudo_tiled` branch.
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        t.node_mut(a).client.as_mut().unwrap().floating_rectangle = Rect::new(0, 0, 100, 50);
        t.set_state(a, ClientState::PseudoTiled);
        let mrect = Rect::new(0, 0, 400, 200);
        let centred = LayoutOptions { center_pseudo_tiled: true, ..Default::default() };
        t.apply_layout(t.root, mrect, 0, Layout::Tiled, mrect, centred);
        let r = t.node(a).client.as_ref().unwrap().tiled_rectangle;
        assert_eq!((r.width, r.height), (100, 50));
        assert_eq!((r.x, r.y), (0 - 1 + (400 - 100) / 2, 0 - 1 + (200 - 50) / 2));
        t.apply_layout(t.root, mrect, 0, Layout::Tiled, mrect, LayoutOptions::default());
        assert_eq!(t.node(a).client.as_ref().unwrap().tiled_rectangle.x, 0);
    }

    #[test]
    fn next_node_and_prev_node_walk_in_order_including_the_splits() {
        // bspwm: `next_node()`/`prev_node()`: first subtree, the node, second subtree.
        let s = settings();
        let mut t = Tree::new();
        let a = insert_client(&mut t, &s, None, 1);
        let b = insert_client(&mut t, &s, Some(a), 2);
        let split = t.root.unwrap();
        assert_eq!(t.next_node(Some(a)), Some(split));
        assert_eq!(t.next_node(Some(split)), Some(b));
        assert_eq!(t.next_node(Some(b)), None);
        assert_eq!(t.prev_node(Some(b)), Some(split));
        assert_eq!(t.prev_node(Some(split)), Some(a));
        assert_eq!(t.prev_node(Some(a)), None);
        assert_eq!(t.next_node(None), None);
    }
}
