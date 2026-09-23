//! Per-frame rendering: client surfaces (via `bsp-core`-computed
//! positions already applied to the `Space`) plus solid-color border
//! rectangles, composited with Smithay's damage tracker.
//!
//! bspwm draws borders as plain colored rectangles with no blur, shadow
//! or animation (`docs/design.md`, Performance budget). Each border is
//! four separate thin strips (top/bottom/left/right) framing the content
//! rectangle rather than one rectangle drawn behind it: `space::render_output`
//! always draws its `custom_elements` argument on top of window content
//! (elements earlier in the render list are topmost), so a single
//! full-sized rectangle behind a window would need a different render
//! path to stay hidden under the content; four non-overlapping strips
//! sidestep that instead.

use smithay::backend::renderer::damage::{
    Error as DamageTrackerError, OutputDamageTracker, RenderOutputResult,
};
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Id, Kind, RenderElement};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Color32F, ImportAll, ImportMem, Renderer, RendererSuper, Texture};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::desktop::layer_map_for_output;
use smithay::desktop::space::SpaceRenderElements;
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::desktop::{Space, Window};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use bsp_core::id::WindowId;
use bsp_core::wm::Wm;

// A window's own render elements plus this crate's border strips,
// combined into one list — the shape both the winit backend (fed
// through `OutputDamageTracker::render_output`) and the DRM backend
// (fed straight into `DrmOutput::render_frame`) need, since neither
// accepts a `Space` directly the way Smithay's own convenience
// `desktop::space::render_output` free function does. Modeled on
// Smithay's reference compositor anvil's own `render.rs`
// `OutputRenderElements` (`docs/bsp-compositor.md` Hardware backend progress) —
// Smithay's *own* internal type of the same name and role
// (`desktop::space::mod.rs`) is not `pub`, so every consumer defines
// its own via this same macro, not by reusing Smithay's.
smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem;
    Cursor = crate::cursor::CursorElement<R>,
    Layer = smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>,
    Space = SpaceRenderElements<R, E>,
    Border = SolidColorRenderElement,
}

/// A `#rrggbb` setting as a render color; `fallback` if it does not parse.
fn setting_color(hex: &str, fallback: Color32F) -> Color32F {
    match bsp_core::settings::parse_hex_color(hex) {
        Some([r, g, b]) => Color32F::new(f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0),
        None => fallback,
    }
}

/// Background clear color (bspwm has no desktop background of its own).
pub const CLEAR_COLOR: Color32F = Color32F::new(0.08, 0.08, 0.08, 1.0);

/// One border strip as the renderer last saw it. The damage tracker tells a
/// changed element from an unchanged one by its `Id` and `CommitCounter`, so a
/// strip keeps both across frames and only bumps the counter when its geometry
/// or colour changes; with fresh ids every frame the whole screen was
/// re-drawn (and re-queued for scanout) on every input event.
struct Strip {
    id: Id,
    commit: CommitCounter,
    geo: Rectangle<i32, Physical>,
    color: Color32F,
}

/// The strips of every window, keyed by window and side (top, bottom, left,
/// right). The compositor is single-threaded, so this lives beside the
/// renderer instead of being threaded through every caller of
/// [`output_elements`].
type BorderCache = HashMap<(WindowId, u8), Strip>;

thread_local! {
    static BORDERS: RefCell<BorderCache> = RefCell::new(HashMap::new());
    /// The preselection feedback rectangles, by monitor and node, kept for the
    /// same reason as the border strips.
    static PRESELS: RefCell<HashMap<(usize, bsp_core::id::NodeId), Strip>> = RefCell::new(HashMap::new());
}

/// The feedback rectangles of `monitor`'s shown desktop: for each node with a
/// preselection, the part of its rectangle the next window will take, in
/// `presel_feedback_color`. Nothing when `presel_feedback` is off.
///
/// bspwm: `src/tree.c` `draw_presel_feedback()`.
fn presel_elements(wm: &Wm, monitor: usize, output_origin: Point<i32, Logical>, scale: Scale<f64>) -> Vec<SolidColorRenderElement> {
    let mut out = Vec::new();
    PRESELS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let mut seen = Vec::new();
        let m = &wm.monitors[monitor];
        // bspwm: none in a monocle desktop (`d->user_layout == LAYOUT_MONOCLE`).
        let shown = m.focused.and_then(|di| m.desktops.get(di)).filter(|d| d.user_layout != bsp_core::tree::Layout::Monocle);
        if let (true, Some(d)) = (wm.settings.presel_feedback, shown) {
            let color = setting_color(&wm.settings.presel_feedback_color, Color32F::new(0.96, 0.84, 0.46, 1.0));
            for id in d.tree.node_ids() {
                let node = d.tree.node(id);
                let Some(presel) = node.presel else { continue };
                let gap = if wm.settings.gapless_monocle && d.layout == bsp_core::tree::Layout::Monocle { 0 } else { d.window_gap };
                let bsp_core::geometry::Rect { x, y, width: w, height: h } = bsp_core::tree::presel_rect(node.rect, presel, gap);
                let geo: Rectangle<i32, Physical> = Rectangle::new(Point::from((x - output_origin.x, y - output_origin.y)), Size::from((w.max(0), h.max(0))))
                    .to_f64()
                    .to_physical(scale)
                    .to_i32_round();
                let key = (monitor, id);
                seen.push(key);
                let entry = cache.entry(key).or_insert_with(|| Strip { id: Id::new(), commit: CommitCounter::default(), geo, color });
                if entry.geo != geo || entry.color != color {
                    entry.geo = geo;
                    entry.color = color;
                    entry.commit.increment();
                }
                out.push(SolidColorRenderElement::new(entry.id.clone(), geo, entry.commit, color, Kind::Unspecified));
            }
        }
        cache.retain(|k, _| k.0 != monitor || seen.contains(k));
    });
    out
}

/// The strip for (`window`, `side`) at `geo` in `color`, reusing the cached
/// element's identity when nothing changed.
fn strip(cache: &mut BorderCache, window: WindowId, side: u8, geo: Rectangle<i32, Physical>, color: Color32F) -> SolidColorRenderElement {
    let entry = cache.entry((window, side)).or_insert_with(|| Strip { id: Id::new(), commit: CommitCounter::default(), geo, color });
    if entry.geo != geo || entry.color != color {
        entry.geo = geo;
        entry.color = color;
        entry.commit.increment();
    }
    SolidColorRenderElement::new(entry.id.clone(), geo, entry.commit, color, Kind::Unspecified)
}

/// Set on a managed window's user data by `WindowAdapter::insert`, so the
/// renderer can tell which client a `Space` element belongs to (for its border).
pub struct WindowKey(pub WindowId);

/// The index of the `bsp-core` monitor `output` shows (they share a name).
pub(crate) fn monitor_of(wm: &Wm, output: &Output) -> Option<usize> {
    let name = output.name();
    wm.monitors.iter().position(|m| m.name == name)
}

/// Builds four border strips (top, bottom, left, right) per window, keyed by
/// window, of the focused desktop of `monitor`, framing its content rectangle without
/// overlapping it, in `output`-relative coordinates.
///
/// A desktop's focused node is drawn with `focused_border_color` on the
/// focused monitor and `active_border_color` on any other; everything else
/// with `normal_border_color`. The width is the one the layout settled on
/// (`Client::shown_border_width`: none for a fullscreen window, or under
/// `borderless_monocle`/`borderless_singleton`).
///
/// bspwm: `src/window.c` `get_border_color()`, `src/tree.c` `apply_layout()`.
fn border_elements(wm: &Wm, monitor: usize, output_origin: Point<i32, Logical>, scale: Scale<f64>) -> HashMap<WindowId, Vec<SolidColorRenderElement>> {
    let black = Color32F::new(0.0, 0.0, 0.0, 1.0);
    let normal = setting_color(&wm.settings.normal_border_color, black);
    let active = setting_color(&wm.settings.active_border_color, black);
    let focused = setting_color(&wm.settings.focused_border_color, black);
    let mut elements: HashMap<WindowId, Vec<SolidColorRenderElement>> = HashMap::new();
    BORDERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let m = &wm.monitors[monitor];
        // Only a monitor's focused desktop is on screen.
        if let Some(d) = m.focused.and_then(|di| m.desktops.get(di)) {
            let mut n = d.tree.first_extrema(d.tree.root);
            while let Some(id) = n {
                let node = d.tree.node(id);
                if let Some(client) = &node.client {
                    let bw = client.shown_border_width;
                    if bw > 0 && !node.hidden {
                        // The rectangle the window is actually shown at.
                        let r = client.shown_rectangle();
                        let color = if d.tree.focus != Some(id) {
                            normal
                        } else if wm.focused_monitor == Some(monitor) {
                            focused
                        } else {
                            active
                        };
                        let to_physical = |x: i32, y: i32, w: i32, h: i32| -> Rectangle<i32, Physical> {
                            Rectangle::new(Point::from((x - output_origin.x, y - output_origin.y)), Size::from((w.max(0), h.max(0))))
                                .to_f64()
                                .to_physical(scale)
                                .to_i32_round()
                        };
                        // Top and bottom strips span the full bordered
                        // width (including the corners); left and right
                        // fill in only the remaining height between them.
                        let w = client.window;
                        elements.insert(
                            w,
                            vec![
                                strip(&mut cache, w, 0, to_physical(r.x - bw, r.y - bw, r.width + 2 * bw, bw), color),
                                strip(&mut cache, w, 1, to_physical(r.x - bw, r.y + r.height, r.width + 2 * bw, bw), color),
                                strip(&mut cache, w, 2, to_physical(r.x - bw, r.y, bw, r.height), color),
                                strip(&mut cache, w, 3, to_physical(r.x + r.width, r.y, bw, r.height), color),
                            ],
                        );
                    }
                }
                n = d.tree.next_leaf(Some(id), d.tree.root);
            }
        }
        // Forget the strips of windows that are gone.
        let alive: HashSet<WindowId> = wm
            .monitors
            .iter()
            .flat_map(|m| &m.desktops)
            .flat_map(|d| {
                let mut windows = Vec::new();
                let mut n = d.tree.first_extrema(d.tree.root);
                while let Some(id) = n {
                    if let Some(c) = &d.tree.node(id).client {
                        windows.push(c.window);
                    }
                    n = d.tree.next_leaf(Some(id), d.tree.root);
                }
                windows
            })
            .collect();
        cache.retain(|(w, _), _| alive.contains(w));
    });
    elements
}

/// Render elements for `output`'s layer surfaces on the given `layers`,
/// in that order (earlier = topmost).
fn layer_elements<R, E>(
    output: &Output,
    renderer: &mut R,
    scale: Scale<f64>,
    layers: &[Layer],
) -> Vec<OutputRenderElements<R, E>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
    E: RenderElement<R>,
{
    let map = layer_map_for_output(output);
    let mut elements = Vec::new();
    for &wanted in layers {
        for layer in map.layers_on(wanted).rev() {
            let Some(geo) = map.layer_geometry(layer) else {
                continue;
            };
            let location = geo.loc.to_f64().to_physical(scale).to_i32_round();
            elements.extend(
                layer
                    .render_elements::<WaylandSurfaceRenderElement<R>>(renderer, location, scale, 1.0)
                    .into_iter()
                    .map(OutputRenderElements::Layer),
            );
        }
    }
    elements
}

/// Whether an unmanaged X11 window (override-redirect, which is what many games
/// and Wine use for fullscreen) covers all of `output`.
pub(crate) fn unmanaged_covers(space: &Space<Window>, output: &Output) -> bool {
    let Some(screen) = space.output_geometry(output) else {
        return false;
    };
    space.elements().any(|w| {
        w.x11_surface().is_some_and(|x| x.is_override_redirect())
            && space.element_geometry(w).is_some_and(|g| g.contains_rect(screen))
    })
}

/// Whether the shown desktop of monitor `monitor` has a fullscreen window
/// (`None`: an output no monitor stands for, which has none).
pub(crate) fn has_fullscreen(wm: &Wm, monitor: Option<usize>) -> bool {
    let Some(m) = monitor.and_then(|mi| wm.monitors.get(mi)) else {
        return false;
    };
    let Some(d) = m.focused.and_then(|di| m.desktops.get(di)) else {
        return false;
    };
    let mut n = d.tree.first_extrema(d.tree.root);
    while let Some(id) = n {
        let node = d.tree.node(id);
        if !node.hidden && node.client.as_ref().is_some_and(|c| c.state == bsp_core::node::ClientState::Fullscreen) {
            return true;
        }
        n = d.tree.next_leaf(Some(id), d.tree.root);
    }
    false
}

/// Builds one frame's full render element list for `output`: border
/// strips (always on top, see this module's doc comment for why) plus
/// every window's own elements — the backend-agnostic half of
/// rendering, shared by the winit driver ([`render_frame`], below,
/// still damage-tracked) and the DRM driver
/// (`crate::udev_backend::render_surface`, which feeds this straight
/// into `DrmOutput::render_frame` — that call does its own internal
/// damage tracking, so it needs no `OutputDamageTracker` of its own).
///
/// `Ok(None)` (not a hard rule 3 `.expect()`/`.unwrap()`) if `output`
/// has no mode set yet — `space_render_elements` cannot enumerate a
/// space's elements without one; a caller mid-way through output setup
/// (mode not applied yet) just skips this frame instead of panicking.
pub fn output_elements<R>(
    output: &Output,
    space: &Space<Window>,
    wm: &Wm,
    renderer: &mut R,
    cursor: Vec<crate::cursor::CursorElement<R>>,
) -> Option<Vec<OutputRenderElements<R, <Window as AsRenderElements<R>>::RenderElement>>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
{
    let scale = Scale::from(output.current_scale().fractional_scale());
    // A locked session shows the lock surface (or black) and nothing else.
    if crate::session_lock::is_locked(output) {
        let mut elements: Vec<OutputRenderElements<R, _>> =
            cursor.into_iter().map(OutputRenderElements::Cursor).collect();
        if let Some(surface) = crate::session_lock::lock_surface(output) {
            elements.extend(
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree::<R, WaylandSurfaceRenderElement<R>>(
                    renderer,
                    &surface,
                    (0, 0),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(OutputRenderElements::Layer),
            );
        }
        return Some(elements);
    }
    // The cursor goes first: earlier elements are topmost.
    let mut elements: Vec<OutputRenderElements<R, _>> =
        cursor.into_iter().map(OutputRenderElements::Cursor).collect();
    // Overlay and top layers, above windows and their borders; the top layer
    // (a bar) goes under a fullscreen window, as in wlroots compositors.
    let monitor = monitor_of(wm, output);
    let fullscreen = has_fullscreen(wm, monitor) || unmanaged_covers(space, output);
    {
        static LAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if LAST.swap(fullscreen, std::sync::atomic::Ordering::Relaxed) != fullscreen {
            tracing::debug!(fullscreen, "fullscreen layering changed");
        }
    }
    let above: &[Layer] = if fullscreen { &[Layer::Overlay] } else { &[Layer::Overlay, Layer::Top] };
    elements.extend(layer_elements(output, renderer, scale, above));
    let Some(output_geo) = space.output_geometry(output) else {
        tracing::debug!("skipping a frame: the output has no mode yet");
        return None;
    };
    // No borders over a fullscreen window: a fullscreen window has none, and the
    // borders of the windows behind it would be drawn on top of it.
    let mut borders = match monitor {
        Some(mi) if !fullscreen => border_elements(wm, mi, output_geo.loc, scale),
        _ => HashMap::new(),
    };
    // The preselection feedback goes just above the topmost tiled window, so
    // floating windows cover it (bspwm: `restack_presel_feedbacks()`).
    let mut presels = match (monitor, fullscreen) {
        (Some(mi), false) => presel_elements(wm, mi, output_geo.loc, scale),
        _ => Vec::new(),
    };
    let tiled: HashSet<WindowId> = monitor
        .and_then(|mi| {
            let m = &wm.monitors[mi];
            m.focused.and_then(|di| m.desktops.get(di))
        })
        .map(|d| {
            d.tree
                .node_ids()
                .into_iter()
                .filter_map(|n| d.tree.node(n).client.as_ref().filter(|c| c.state.is_tiled()).map(|c| c.window))
                .collect()
        })
        .unwrap_or_default();
    // The windows only (`space_render_elements` would draw the output's layer
    // surfaces too, which `layer_elements` already does with its own stacking),
    // topmost first. A window's border goes right after its own content, so it
    // is covered by the windows above it and covers the ones below, exactly
    // like the window it frames (bspwm: the border is part of the X window).
    for window in space.elements().rev() {
        let (Some(bbox), Some(location)) = (space.element_bbox(window), space.element_location(window)) else {
            continue;
        };
        if !output_geo.overlaps(bbox) {
            continue;
        }
        if !presels.is_empty() && window.user_data().get::<WindowKey>().is_some_and(|k| tiled.contains(&k.0)) {
            elements.extend(presels.drain(..).map(OutputRenderElements::Border));
        }
        let render_location = location - window.geometry().loc - output_geo.loc;
        elements.extend(
            window
                .render_elements::<<Window as AsRenderElements<R>>::RenderElement>(renderer, render_location.to_physical_precise_round(scale), scale, 1.0)
                .into_iter()
                .map(|e| OutputRenderElements::Space(SpaceRenderElements::Element(smithay::backend::renderer::element::Wrap::from(e)))),
        );
        if let Some(strips) = window.user_data().get::<WindowKey>().and_then(|k| borders.remove(&k.0)) {
            elements.extend(strips.into_iter().map(OutputRenderElements::Border));
        }
    }
    // No tiled window: the feedback goes under the windows.
    elements.extend(presels.into_iter().map(OutputRenderElements::Border));
    // Bottom and background layers, under everything.
    let below: &[Layer] = if fullscreen {
        &[Layer::Top, Layer::Bottom, Layer::Background]
    } else {
        &[Layer::Bottom, Layer::Background]
    };
    elements.extend(layer_elements(output, renderer, scale, below));
    Some(elements)
}

/// Renders one frame for `output`: client surfaces plus border strips,
/// composited against [`CLEAR_COLOR`] with damage tracking. The winit
/// (nested) backend's own driver — see [`output_elements`] for the half
/// of this shared with the DRM backend. `nested`-only in practice (this
/// module stays unconditionally compiled since `output_elements` is
/// shared, so a `real`-only build would otherwise warn on this function
/// specifically as unreachable dead code).
#[cfg_attr(not(feature = "nested"), allow(dead_code))]
pub fn render_frame<'d>(
    output: &Output,
    space: &Space<Window>,
    wm: &Wm,
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    damage_tracker: &'d mut OutputDamageTracker,
    age: usize,
) -> Result<RenderOutputResult<'d>, DamageTrackerError<<GlesRenderer as RendererSuper>::Error>> {
    let elements = output_elements(output, space, wm, renderer, Vec::new()).unwrap_or_default();
    damage_tracker.render_output(renderer, framebuffer, age, &elements, CLEAR_COLOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::renderer::element::Element;

    #[test]
    fn a_border_strip_keeps_its_identity_until_it_changes() {
        // The damage tracker re-draws an element whose id or commit changed; a
        // border that did not change must not count as damage.
        let mut cache = BorderCache::new();
        let geo = Rectangle::new((0, 0).into(), (10, 1).into());
        let red = Color32F::new(1.0, 0.0, 0.0, 1.0);
        let a = strip(&mut cache, WindowId(1), 0, geo, red);
        let b = strip(&mut cache, WindowId(1), 0, geo, red);
        assert_eq!(a.id(), b.id());
        assert_eq!(a.current_commit(), b.current_commit());

        let moved = strip(&mut cache, WindowId(1), 0, Rectangle::new((5, 0).into(), (10, 1).into()), red);
        assert_eq!(a.id(), moved.id());
        assert_ne!(a.current_commit(), moved.current_commit());

        let recoloured = strip(&mut cache, WindowId(1), 0, moved.geometry(smithay::utils::Scale::from(1.0)), Color32F::new(0.0, 1.0, 0.0, 1.0));
        assert_ne!(moved.current_commit(), recoloured.current_commit());

        let other_side = strip(&mut cache, WindowId(1), 1, geo, red);
        assert_ne!(a.id(), other_side.id());
    }
}
