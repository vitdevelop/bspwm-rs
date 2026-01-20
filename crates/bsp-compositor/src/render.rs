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
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Color32F, RendererSuper};
use smithay::desktop::space::render_output;
use smithay::desktop::{Space, Window};
use smithay::output::Output;
use smithay::utils::{Physical, Point, Rectangle, Scale, Size};

use bsp_core::wm::Wm;

/// The color a window's border is drawn with: focused if `focused`.
///
/// bspwm's default `focused_border_color`/`normal_border_color`
/// (`src/settings.c`) are both shades of grey; `bsp-core::Settings` does
/// not carry colors yet (`docs/bsp-core.md`: colors are an X11/pointer
/// concern left to `bsp-compositor`), so these are hardcoded for now.
const FOCUSED_BORDER_COLOR: Color32F = Color32F::new(0.29, 0.55, 0.86, 1.0);
const NORMAL_BORDER_COLOR: Color32F = Color32F::new(0.35, 0.35, 0.35, 1.0);

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
fn border_elements(wm: &Wm, scale: Scale<f64>) -> Vec<SolidColorRenderElement> {
    let mut elements = Vec::new();
    for m in &wm.monitors {
        for d in &m.desktops {
            let mut n = d.tree.first_extrema(d.tree.root);
            while let Some(id) = n {
                let node = d.tree.node(id);
                if let Some(client) = &node.client {
                    let bw = client.border_width;
                    if bw > 0 && !node.hidden {
                        let r = client.tiled_rectangle;
                        let color = if d.tree.focus == Some(id) {
                            FOCUSED_BORDER_COLOR
                        } else {
                            NORMAL_BORDER_COLOR
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

/// Renders one frame for `output`: client surfaces plus border strips,
/// composited against [`CLEAR_COLOR`] with damage tracking.
pub fn render_frame<'d>(
    output: &Output,
    space: &Space<Window>,
    wm: &Wm,
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    damage_tracker: &'d mut OutputDamageTracker,
    age: usize,
) -> Result<RenderOutputResult<'d>, DamageTrackerError<<GlesRenderer as RendererSuper>::Error>> {
    let scale = Scale::from(output.current_scale().fractional_scale());
    let borders = border_elements(wm, scale);
    render_output(
        output,
        renderer,
        framebuffer,
        1.0,
        age,
        [space],
        &borders,
        damage_tracker,
        CLEAR_COLOR,
    )
}
