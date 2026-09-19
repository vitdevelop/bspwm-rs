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
use smithay::utils::{Physical, Point, Rectangle, Scale, Size};

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

fn strip(geo: Rectangle<i32, Physical>, color: Color32F) -> SolidColorRenderElement {
    SolidColorRenderElement::new(
        Id::new(),
        geo,
        CommitCounter::default(),
        color,
        Kind::Unspecified,
    )
}

/// Builds four border strips (top, bottom, left, right) per tiled client,
/// framing its content rectangle without overlapping it.
///
/// A desktop's focused node is drawn with `focused_border_color` on the
/// focused monitor and `active_border_color` on any other; everything else
/// with `normal_border_color`.
///
/// bspwm: `src/window.c` `get_border_color()`.
fn border_elements(wm: &Wm, scale: Scale<f64>) -> Vec<SolidColorRenderElement> {
    let black = Color32F::new(0.0, 0.0, 0.0, 1.0);
    let normal = setting_color(&wm.settings.normal_border_color, black);
    let active = setting_color(&wm.settings.active_border_color, black);
    let focused = setting_color(&wm.settings.focused_border_color, black);
    let mut elements = Vec::new();
    for (mi, m) in wm.monitors.iter().enumerate() {
        for (di, d) in m.desktops.iter().enumerate() {
            // Only a monitor's focused desktop is on screen.
            if m.focused != Some(di) {
                continue;
            }
            let mut n = d.tree.first_extrema(d.tree.root);
            while let Some(id) = n {
                let node = d.tree.node(id);
                if let Some(client) = &node.client {
                    let bw = client.border_width;
                    if bw > 0 && !node.hidden {
                        let r = client.tiled_rectangle;
                        let color = if d.tree.focus != Some(id) {
                            normal
                        } else if wm.focused_monitor == Some(mi) {
                            focused
                        } else {
                            active
                        };
                        let to_physical = |x: i32,
                                           y: i32,
                                           w: i32,
                                           h: i32|
                         -> Rectangle<i32, Physical> {
                            Rectangle::new(Point::from((x, y)), Size::from((w.max(0), h.max(0))))
                                .to_f64()
                                .to_physical(scale)
                                .to_i32_round()
                        };
                        // Top and bottom strips span the full bordered
                        // width (including the corners); left and right
                        // fill in only the remaining height between them.
                        elements.push(strip(
                            to_physical(r.x - bw, r.y - bw, r.width + 2 * bw, bw),
                            color,
                        ));
                        elements.push(strip(
                            to_physical(r.x - bw, r.y + r.height, r.width + 2 * bw, bw),
                            color,
                        ));
                        elements.push(strip(to_physical(r.x - bw, r.y, bw, r.height), color));
                        elements.push(strip(to_physical(r.x + r.width, r.y, bw, r.height), color));
                    }
                }
                n = d.tree.next_leaf(Some(id), d.tree.root);
            }
        }
    }
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
fn unmanaged_covers(space: &Space<Window>, output: &Output) -> bool {
    let Some(screen) = space.output_geometry(output) else {
        return false;
    };
    space.elements().any(|w| {
        w.x11_surface().is_some_and(|x| x.is_override_redirect())
            && space.element_geometry(w).is_some_and(|g| g.contains_rect(screen))
    })
}

/// Whether the focused desktop of the focused monitor shows a fullscreen window.
fn has_fullscreen(wm: &Wm) -> bool {
    let Some(d) = wm.focused_monitor.and_then(|mi| wm.monitors.get(mi)).and_then(|m| m.focused.and_then(|di| m.desktops.get(di))) else {
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
    let fullscreen = has_fullscreen(wm) || unmanaged_covers(space, output);
    {
        static LAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if LAST.swap(fullscreen, std::sync::atomic::Ordering::Relaxed) != fullscreen {
            tracing::debug!(fullscreen, "fullscreen layering changed");
        }
    }
    let above: &[Layer] = if fullscreen { &[Layer::Overlay] } else { &[Layer::Overlay, Layer::Top] };
    elements.extend(layer_elements(output, renderer, scale, above));
    // No borders over a fullscreen window: a fullscreen window has none, and the
    // borders of the windows behind it would be drawn on top of it.
    if !fullscreen {
        elements.extend(border_elements(wm, scale).into_iter().map(OutputRenderElements::Border));
    }
    // The windows only: `space_render_elements` would draw the output's layer
    // surfaces too (bars above every window, whatever the order below), which
    // `layer_elements` already does with its own stacking.
    let Some(output_geo) = space.output_geometry(output) else {
        tracing::debug!("skipping a frame: the output has no mode yet");
        return None;
    };
    let space_elements = space.render_elements_for_region(renderer, &output_geo, scale, 1.0);
    elements.extend(
        space_elements
            .into_iter()
            .map(|e| OutputRenderElements::Space(SpaceRenderElements::Element(smithay::backend::renderer::element::Wrap::from(e)))),
    );
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
