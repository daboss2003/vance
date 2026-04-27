//! Viewport bounds — physical pixels, top-left origin, as the chrome reports them.

/// Physical-pixel bounds of the viewport rectangle inside the host window.
///
/// Origin is top-left of the host window's client area (matches the chrome's
/// `getBoundingClientRect()` × `devicePixelRatio` calculation). All four
/// fields are unsigned because the chrome only reports valid rectangles —
/// any negative result clips at zero on the JS side before reaching us.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewportBounds {
    /// Distance from the host window's left edge, in physical pixels.
    pub x: u32,
    /// Distance from the host window's top edge, in physical pixels.
    pub y: u32,
    /// Width in physical pixels.
    pub w: u32,
    /// Height in physical pixels.
    pub h: u32,
}

impl ViewportBounds {
    /// Construct from raw integer values.
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Returns true if either dimension is zero (the chrome has not yet
    /// reported real bounds, or the placeholder div is hidden).
    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}
