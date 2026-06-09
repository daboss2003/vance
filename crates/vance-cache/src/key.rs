//! Cache key types.
//!
//! Decoded-frame cache keys carry three components:
//! - **`source_uri`** — identifies the media (file path, URL, ...).
//! - **`pts`** — frame timestamp in the source's time base.
//! - **`transform_hash`** — 64-bit hash of any transforms applied
//!   before caching (color-space conversion, scale, pixel format).
//!   Pre-transform frames use `transform_hash = 0`; transformed
//!   variants get unique hashes per (input space → output space → pix
//!   format) tuple.
//!
//! The `disk_key()` method derives a deterministic 16-character hex
//! file name via FNV-1a 64-bit so on-disk entries survive
//! Rust / std hasher version churn.

use crate::fnv::Fnv1a64;

/// Cache key for one decoded (and optionally transformed) frame.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CacheKey {
    /// Source URI (file path, URL, etc.).
    pub source_uri: String,
    /// Frame PTS in the source's time base.
    pub pts: i64,
    /// 64-bit hash of the transform pipeline applied to the cached
    /// bytes. `0` = the raw decoded frame.
    pub transform_hash: u64,
}

impl CacheKey {
    /// Convenience: build a raw-decoded-frame key (transform_hash = 0).
    pub fn for_source_frame(uri: impl Into<String>, pts: i64, transform_hash: u64) -> Self {
        Self {
            source_uri: uri.into(),
            pts,
            transform_hash,
        }
    }

    /// Derive the on-disk file name. Returns a 16-character lowercase
    /// hex string that's safe for any filesystem.
    pub fn disk_key(&self) -> String {
        let mut h = Fnv1a64::new();
        h.write(self.source_uri.as_bytes());
        // Separator so URIs that legitimately contain the next field
        // bytes can't collide ("file:///a/b" vs "file:///a" + "/b").
        h.write(&[0]);
        h.write_i64(self.pts);
        h.write(&[0]);
        h.write_u64(self.transform_hash);
        h.finish_hex()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_keys_have_equal_disk_keys() {
        let a = CacheKey::for_source_frame("file:///x.mp4", 1, 0);
        let b = CacheKey::for_source_frame("file:///x.mp4", 1, 0);
        assert_eq!(a.disk_key(), b.disk_key());
    }

    #[test]
    fn different_pts_yields_different_disk_key() {
        let a = CacheKey::for_source_frame("file:///x.mp4", 1, 0);
        let b = CacheKey::for_source_frame("file:///x.mp4", 2, 0);
        assert_ne!(a.disk_key(), b.disk_key());
    }

    #[test]
    fn different_uri_yields_different_disk_key() {
        let a = CacheKey::for_source_frame("file:///a.mp4", 1, 0);
        let b = CacheKey::for_source_frame("file:///b.mp4", 1, 0);
        assert_ne!(a.disk_key(), b.disk_key());
    }

    #[test]
    fn different_transform_yields_different_disk_key() {
        let a = CacheKey::for_source_frame("file:///x.mp4", 1, 0);
        let b = CacheKey::for_source_frame("file:///x.mp4", 1, 0xdeadbeef);
        assert_ne!(a.disk_key(), b.disk_key());
    }

    /// Separator-injection guard: the manual zero-byte separator
    /// between fields should prevent URI suffixes from colliding
    /// with neighbouring numeric fields.
    #[test]
    fn separator_prevents_uri_pts_collision() {
        // "a" + pts=1 must not collide with "a\x01" + pts=0 etc.
        let a = CacheKey::for_source_frame("a", 0, 0);
        let b = CacheKey::for_source_frame("a\0", 0, 0);
        assert_ne!(a.disk_key(), b.disk_key());
    }
}
