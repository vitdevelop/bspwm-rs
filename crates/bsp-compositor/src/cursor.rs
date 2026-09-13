#![cfg_attr(not(feature = "real"), allow(dead_code))]
//! The mouse cursor as render elements.
//!
//! A DRM/KMS backend owns the whole screen, so nothing draws the pointer
//! for it (the nested winit backend's host compositor does): this module
//! builds the cursor's render elements — either the named default cursor
//! from the user's xcursor theme, or the surface a client set through
//! `wl_pointer.set_cursor` — for `crate::render::output_elements` to put
//! on top of everything else. Modeled on Smithay's reference compositor
//! anvil's `drawing.rs` `PointerElement`/`cursor.rs`, minus cursor
//! animation (the first frame of the theme's cursor is used).

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ImportAll, ImportMem, Renderer, Texture};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::utils::{Logical, Point, Scale, Transform};
use smithay::wayland::compositor::with_states;

/// Preferred cursor size in pixels when `XCURSOR_SIZE` is unset.
const DEFAULT_SIZE: u32 = 24;

smithay::backend::renderer::element::render_elements! {
    /// One cursor render element: the theme's named cursor, or a client's cursor surface.
    pub CursorElement<R> where R: ImportAll + ImportMem;
    Named = MemoryRenderBufferRenderElement<R>,
    Surface = WaylandSurfaceRenderElement<R>,
}

/// The theme's default arrow, decoded once at startup.
pub struct CursorImages {
    buffer: MemoryRenderBuffer,
    hotspot: Point<i32, Logical>,
}

impl CursorImages {
    /// Loads the default cursor from the xcursor theme named by
    /// `XCURSOR_THEME` (else `default`) at `XCURSOR_SIZE` (else 24).
    /// Falls back to a small solid square if no theme provides one, so
    /// the pointer is always visible.
    pub fn load() -> Self {
        let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_SIZE);
        let theme = xcursor::CursorTheme::load(&theme_name);
        let loaded = ["default", "left_ptr", "arrow"].iter().find_map(|name| {
            let path = theme.load_icon(name)?;
            let data = std::fs::read(path).ok()?;
            let images = xcursor::parser::parse_xcursor(&data)?;
            images
                .into_iter()
                .min_by_key(|i| (i.size as i64 - size as i64).abs())
        });
        match loaded {
            Some(image) => {
                tracing::info!(theme = theme_name, size = image.size, "loaded the default cursor");
                Self {
                    buffer: MemoryRenderBuffer::from_slice(
                        &image.pixels_rgba,
                        Fourcc::Abgr8888,
                        (image.width as i32, image.height as i32),
                        1,
                        Transform::Normal,
                        None,
                    ),
                    hotspot: (image.xhot as i32, image.yhot as i32).into(),
                }
            }
            None => {
                tracing::warn!(theme = theme_name, "no xcursor theme found; using a plain square cursor");
                let pixels = vec![0xFFu8; 12 * 12 * 4];
                Self {
                    buffer: MemoryRenderBuffer::from_slice(
                        &pixels,
                        Fourcc::Abgr8888,
                        (12, 12),
                        1,
                        Transform::Normal,
                        None,
                    ),
                    hotspot: (0, 0).into(),
                }
            }
        }
    }
}

/// The cursor's render elements for one output. `location` is the
/// pointer position in that output's own logical coordinates.
pub fn cursor_elements<R>(
    renderer: &mut R,
    images: &CursorImages,
    status: &CursorImageStatus,
    location: Point<f64, Logical>,
    scale: Scale<f64>,
) -> Vec<CursorElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Texture + Clone + Send + 'static,
{
    match status {
        CursorImageStatus::Hidden => Vec::new(),
        CursorImageStatus::Named(_) => {
            let at = (location - images.hotspot.to_f64()).to_physical(scale);
            match MemoryRenderBufferRenderElement::from_buffer(renderer, at, &images.buffer, None, None, None, Kind::Cursor) {
                Ok(element) => vec![CursorElement::Named(element)],
                Err(err) => {
                    tracing::warn!("failed to import the cursor: {err}");
                    Vec::new()
                }
            }
        }
        CursorImageStatus::Surface(surface) => {
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .and_then(|data| data.lock().ok().map(|d| d.hotspot))
                    .unwrap_or_default()
            });
            let at = (location - hotspot.to_f64()).to_physical(scale).to_i32_round();
            render_elements_from_surface_tree(renderer, surface, at, scale, 1.0, Kind::Cursor)
        }
    }
}
