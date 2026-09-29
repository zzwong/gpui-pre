//! Visual magnification ("pinch zoom") of a window's rendered output.
//!
//! Magnification enlarges what the window paints without changing layout, like the
//! pinch zoom of a web browser's visual viewport. Elements keep laying out and
//! hit-testing in *content* coordinates (the logical pixels layout produces); the
//! window maps them to *window* coordinates (logical pixels on screen) only when
//! painting, and maps input positions back before dispatching them.
//!
//! ```text
//! window = (content - origin) * scale
//! content = window / scale + origin
//! ```
//!
//! Glyphs and SVGs are rasterized at the magnified device scale, so text stays crisp
//! rather than being a bitmap upscale of the unmagnified frame.

use crate::{Bounds, Pixels, Point, ScaledPixels, Size, point, px};

/// Visual magnification applied to everything a window paints.
///
/// See the [module documentation](self) for the coordinate model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Magnification {
    /// How much the rendered content is enlarged. `1.0` means no magnification.
    pub scale: f32,
    /// The content point shown at the window's top-left corner.
    pub origin: Point<Pixels>,
}

impl Default for Magnification {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The largest magnification a window accepts. Applications usually clamp lower; this
/// bound only keeps glyph rasterization and atlas usage finite.
pub const MAX_MAGNIFICATION: f32 = 8.0;

/// Scroll-wheel "line" deltas are converted to this many window pixels when they pan a
/// magnified window.
pub(crate) const MAGNIFIED_PAN_PIXELS_PER_LINE: f32 = 20.0;

/// Number of raster steps per doubling of scale while a magnification gesture is live.
const LIVE_RASTER_STEPS_PER_OCTAVE: f32 = 4.0;

impl Magnification {
    /// No magnification.
    pub const IDENTITY: Self = Self {
        scale: 1.0,
        origin: Point {
            x: Pixels::ZERO,
            y: Pixels::ZERO,
        },
    };

    /// Magnification by `scale` with `origin` at the window's top-left corner.
    pub fn new(scale: f32, origin: Point<Pixels>) -> Self {
        Self { scale, origin }
    }

    /// Whether this leaves rendering unchanged.
    pub fn is_identity(&self) -> bool {
        self.scale == 1.0 && self.origin == Point::default()
    }

    /// Maps a window point (for example a mouse position) to the content point under it.
    pub fn window_to_content(&self, point: Point<Pixels>) -> Point<Pixels> {
        point.map(|c| c / self.scale) + self.origin
    }

    /// Maps a content point to where it appears in the window.
    pub fn content_to_window(&self, point: Point<Pixels>) -> Point<Pixels> {
        (point - self.origin).map(|c| c * self.scale)
    }

    /// Maps content bounds (for example an IME caret rect) to where they appear in the window.
    pub fn content_to_window_bounds(&self, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        Bounds {
            origin: self.content_to_window(bounds.origin),
            size: bounds.size.map(|c| c * self.scale),
        }
    }

    /// Maps window bounds to the content bounds shown there.
    pub fn window_to_content_bounds(&self, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        Bounds {
            origin: self.window_to_content(bounds.origin),
            size: bounds.size.map(|c| c / self.scale),
        }
    }

    /// The content region visible in a window whose viewport is `viewport`.
    pub fn visible_content(&self, viewport: Size<Pixels>) -> Bounds<Pixels> {
        self.window_to_content_bounds(Bounds::new(Point::default(), viewport))
    }

    /// Returns the magnification at `scale` that keeps `content_anchor` at the same window
    /// position, so the point under the user's fingers stays put while pinching.
    ///
    /// The result is not clamped; pass it through [`Self::clamped`].
    pub fn zoomed_about(&self, content_anchor: Point<Pixels>, scale: f32) -> Self {
        let window_anchor = self.content_to_window(content_anchor);
        Self {
            scale,
            origin: content_anchor - window_anchor.map(|c| c / scale),
        }
    }

    /// Clamps the scale to `1.0..=MAX_MAGNIFICATION` and the origin so the magnified
    /// content still covers the whole viewport, then snaps the origin so the paint
    /// translation is a whole number of device pixels. Snapping keeps pixel-snapped
    /// geometry and glyph subpixel variants identical to what an unpanned frame at the
    /// same scale would produce.
    pub fn clamped(&self, viewport: Size<Pixels>, scale_factor: f32) -> Self {
        let scale = if self.scale.is_finite() {
            self.scale.clamp(1.0, MAX_MAGNIFICATION)
        } else {
            1.0
        };
        let device_scale = scale * scale_factor;
        let clamp_axis = |origin: Pixels, extent: Pixels| {
            let max = (extent - extent / scale).max(Pixels::ZERO);
            let origin = if origin.0.is_finite() {
                origin.clamp(Pixels::ZERO, max)
            } else {
                Pixels::ZERO
            };
            // Round to the device grid, but never past the clamp limit.
            let snapped = px((origin.0 * device_scale).round() / device_scale);
            if snapped > max {
                px((max.0 * device_scale).floor() / device_scale)
            } else {
                snapped
            }
        };
        Self {
            scale,
            origin: point(
                clamp_axis(self.origin.x, viewport.width),
                clamp_axis(self.origin.y, viewport.height),
            ),
        }
    }

    /// Pans the magnified view by a window-space scroll delta (positive moves the content
    /// down/right, like [`crate::ScrollWheelEvent`]) and returns the clamped magnification
    /// with the part of the delta that panning could not absorb, in window pixels.
    ///
    /// Callers forward the leftover to the content, the way a browser scrolls the page
    /// once the pinch-zoomed visual viewport reaches its edge.
    pub fn panned_by(
        &self,
        window_delta: Point<Pixels>,
        viewport: Size<Pixels>,
        scale_factor: f32,
    ) -> (Self, Point<Pixels>) {
        let desired = Self {
            scale: self.scale,
            origin: self.origin - window_delta.map(|c| c / self.scale),
        };
        let panned = desired.clamped(viewport, scale_factor);
        let absorbed = (self.origin - panned.origin).map(|c| c * self.scale);
        let mut remaining = window_delta - absorbed;
        // Sub-device-pixel residue from origin snapping is not a real overscroll.
        let epsilon = px(1.0 / scale_factor.max(1.0));
        if remaining.x.abs() < epsilon {
            remaining.x = Pixels::ZERO;
        }
        if remaining.y.abs() < epsilon {
            remaining.y = Pixels::ZERO;
        }
        (panned, remaining)
    }

    /// The device-pixel translation applied to painted primitives after scaling them by
    /// `scale * scale_factor`. Whole pixels when `self` came from [`Self::clamped`].
    pub fn device_translation(&self, scale_factor: f32) -> Point<ScaledPixels> {
        let device_scale = self.scale * scale_factor;
        self.origin
            .map(|c| ScaledPixels((c.0 * device_scale).round()))
    }

    /// Maps content bounds already scaled to device pixels at `scale * scale_factor` into
    /// device pixels on screen. This is the transform content masks and primitive bounds
    /// go through.
    pub fn translate_device_bounds(
        &self,
        bounds: Bounds<ScaledPixels>,
        scale_factor: f32,
    ) -> Bounds<ScaledPixels> {
        Bounds {
            origin: bounds.origin - self.device_translation(scale_factor),
            size: bounds.size,
        }
    }

    /// The scale glyphs and SVGs are rasterized at, relative to the display scale.
    ///
    /// Exact when settled so text is crisp. While a gesture is `live` it rounds down to a
    /// quarter-octave step, so a pinch reuses a handful of raster sizes (drawn slightly
    /// stretched) instead of rasterizing every glyph again on every frame.
    pub fn raster_scale(&self, live: bool) -> f32 {
        if !live || self.scale <= 1.0 {
            return self.scale;
        }
        let steps = (self.scale.log2() * LIVE_RASTER_STEPS_PER_OCTAVE).floor();
        (steps / LIVE_RASTER_STEPS_PER_OCTAVE).exp2().max(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size;

    const VIEWPORT: Size<Pixels> = Size {
        width: Pixels(800.0),
        height: Pixels(600.0),
    };

    fn assert_close(a: Point<Pixels>, b: Point<Pixels>) {
        assert!(
            (a.x.0 - b.x.0).abs() < 1e-3 && (a.y.0 - b.y.0).abs() < 1e-3,
            "{a:?} != {b:?}"
        );
    }

    #[test]
    fn identity_maps_points_unchanged() {
        let m = Magnification::IDENTITY;
        assert!(m.is_identity());
        let p = point(px(12.5), px(40.0));
        assert_eq!(m.window_to_content(p), p);
        assert_eq!(m.content_to_window(p), p);
        assert_eq!(m.device_translation(2.0), Point::default());
        assert_eq!(m.raster_scale(true), 1.0);
    }

    #[test]
    fn forward_and_inverse_round_trip() {
        let m = Magnification::new(2.5, point(px(100.0), px(60.0)));
        let content = point(px(180.0), px(90.0));
        let window = m.content_to_window(content);
        assert_close(window, point(px(200.0), px(75.0)));
        assert_close(m.window_to_content(window), content);

        let bounds = Bounds::new(content, size(px(10.0), px(4.0)));
        let on_screen = m.content_to_window_bounds(bounds);
        assert_close(on_screen.origin, window);
        assert_eq!(on_screen.size, size(px(25.0), px(10.0)));
        let back = m.window_to_content_bounds(on_screen);
        assert_close(back.origin, bounds.origin);
        assert_close(
            point(back.size.width, back.size.height),
            point(px(10.0), px(4.0)),
        );
    }

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let start = Magnification::new(1.5, point(px(40.0), px(30.0)));
        let anchor = point(px(300.0), px(200.0));
        let before = start.content_to_window(anchor);
        for scale in [1.0, 2.0, 3.7, 5.0] {
            let zoomed = start.zoomed_about(anchor, scale);
            assert_eq!(zoomed.scale, scale);
            assert_close(zoomed.content_to_window(anchor), before);
        }
    }

    #[test]
    fn zoom_from_identity_about_center() {
        let anchor = point(px(400.0), px(300.0));
        let zoomed = Magnification::IDENTITY
            .zoomed_about(anchor, 2.0)
            .clamped(VIEWPORT, 2.0);
        assert_close(zoomed.origin, point(px(200.0), px(150.0)));
        assert_close(zoomed.content_to_window(anchor), anchor);
    }

    #[test]
    fn clamp_limits_scale_and_origin() {
        let too_small = Magnification::new(0.5, point(px(10.0), px(10.0))).clamped(VIEWPORT, 2.0);
        assert_eq!(too_small, Magnification::IDENTITY);

        let too_big = Magnification::new(50.0, Point::default()).clamped(VIEWPORT, 2.0);
        assert_eq!(too_big.scale, MAX_MAGNIFICATION);

        // At 2x only half the viewport is visible, so the origin can be at most half.
        let past_edge = Magnification::new(2.0, point(px(900.0), px(-5.0))).clamped(VIEWPORT, 2.0);
        assert_close(past_edge.origin, point(px(400.0), px(0.0)));
        let visible = past_edge.visible_content(VIEWPORT);
        assert_close(visible.bottom_right(), point(px(800.0), px(300.0)));

        let nan = Magnification::new(f32::NAN, point(px(f32::NAN), px(1.0))).clamped(VIEWPORT, 2.0);
        assert_eq!(nan, Magnification::IDENTITY);
    }

    #[test]
    fn clamp_snaps_translation_to_device_pixels() {
        for scale_factor in [1.0, 1.5, 2.0] {
            let m = Magnification::new(2.3, point(px(123.456), px(78.9)))
                .clamped(VIEWPORT, scale_factor);
            let device = m.origin.map(|c| c.0 * m.scale * scale_factor);
            assert!((device.x - device.x.round()).abs() < 1e-3, "{device:?}");
            assert!((device.y - device.y.round()).abs() < 1e-3, "{device:?}");
            let max = VIEWPORT.width - VIEWPORT.width / m.scale;
            assert!(m.origin.x <= max);
        }
    }

    #[test]
    fn pan_consumes_delta_until_edge_then_returns_remainder() {
        let m = Magnification::new(2.0, point(px(100.0), px(100.0)));
        // Scrolling "down" (negative y) moves the view toward the content bottom.
        let (panned, remaining) = m.panned_by(point(px(0.0), px(-40.0)), VIEWPORT, 2.0);
        assert_close(panned.origin, point(px(100.0), px(120.0)));
        assert_eq!(remaining, Point::default());

        // Only 200 more content px (400 window px) fit before the bottom edge at y = 300.
        let (at_edge, remaining) = panned.panned_by(point(px(0.0), px(-1000.0)), VIEWPORT, 2.0);
        assert_close(at_edge.origin, point(px(100.0), px(300.0)));
        assert_close(remaining, point(px(0.0), px(-640.0)));

        // At 1x there is nothing to pan; the whole delta passes through.
        let (same, remaining) =
            Magnification::IDENTITY.panned_by(point(px(3.0), px(-7.0)), VIEWPORT, 2.0);
        assert_eq!(same, Magnification::IDENTITY);
        assert_close(remaining, point(px(3.0), px(-7.0)));
    }

    #[test]
    fn content_mask_translates_with_primitives() {
        let scale_factor = 2.0;
        let m = Magnification::new(2.0, point(px(50.0), px(25.0))).clamped(VIEWPORT, scale_factor);
        let device_scale = m.scale * scale_factor;
        let mask = Bounds::new(point(px(60.0), px(30.0)), size(px(100.0), px(50.0)));
        let scaled = mask.scale(device_scale);
        let on_screen = m.translate_device_bounds(scaled, scale_factor);
        // Same result as mapping content -> window logically, then to device pixels.
        let expected = m.content_to_window_bounds(mask).scale(scale_factor);
        assert!((on_screen.origin.x.0 - expected.origin.x.0).abs() < 1e-3);
        assert!((on_screen.origin.y.0 - expected.origin.y.0).abs() < 1e-3);
        assert_eq!(on_screen.size, expected.size);
    }

    #[test]
    fn live_raster_scale_quantizes_downward() {
        let settled = Magnification::new(2.7, Point::default());
        assert_eq!(settled.raster_scale(false), 2.7);
        let live = settled.raster_scale(true);
        assert!(live <= 2.7 && live > 2.7 / 2f32.powf(0.25), "{live}");
        // Nearby scales share a raster step.
        assert_eq!(
            Magnification::new(2.75, Point::default()).raster_scale(true),
            live
        );
        assert_eq!(
            Magnification::new(2.0, Point::default()).raster_scale(true),
            2.0
        );
    }
}
