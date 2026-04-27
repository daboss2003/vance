//! Non-macOS placeholder. Phase −1 spike was verified on macOS only;
//! Windows + Linux re-verification is the first task of Phase 0 Week 1.
//! Until that lands, every method here returns `Unimplemented` so the build
//! still passes on CI while the integration code wires things up.

use crate::{HostWindowHandle, Result, ViewportBounds, ViewportError};

/// Stub [`crate::Viewport`] for non-macOS targets. See [`ViewportError::Unimplemented`].
pub struct Viewport {
    _hide_constructor: (),
}

impl Viewport {
    /// Always returns `Err(ViewportError::Unimplemented)`. Real impls land
    /// per platform after the Phase 0 Week 1 spike re-verification (ADR 0005).
    #[allow(unused_variables)]
    pub fn install(host: HostWindowHandle, wgpu_instance: &wgpu::Instance) -> Result<Self> {
        Err(ViewportError::Unimplemented)
    }

    /// Stub.
    pub fn surface(&self) -> &wgpu::Surface<'static> {
        unreachable!("Viewport::install always errors on this platform")
    }

    /// Stub.
    pub fn backing_scale(&self) -> f64 {
        1.0
    }

    /// Stub.
    pub fn set_bounds(&self, _bounds: ViewportBounds) {}

    /// Stub.
    pub fn current_bounds(&self) -> ViewportBounds {
        ViewportBounds::default()
    }
}
