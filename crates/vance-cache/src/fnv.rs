//! 64-bit FNV-1a hash, deterministic across Rust / std versions.
//!
//! Used to content-address on-disk cache entries. NOT cryptographic;
//! the only property we need is low-collision among legitimate keys
//! the editor produces (URI strings + i64 PTS + transform hashes).
//!
//! Reference: <http://www.isthe.com/chongo/tech/comp/fnv/index.html>

/// FNV-1a 64-bit offset basis.
pub const FNV1A64_OFFSET: u64 = 0xcbf29ce484222325;
/// FNV-1a 64-bit prime.
pub const FNV1A64_PRIME: u64 = 0x100000001b3;

/// Streaming 64-bit FNV-1a hasher.
#[derive(Clone, Copy, Debug)]
pub struct Fnv1a64 {
    state: u64,
}

impl Fnv1a64 {
    /// New hasher seeded with the standard FNV-1a offset basis.
    pub fn new() -> Self {
        Self {
            state: FNV1A64_OFFSET,
        }
    }

    /// Consume some bytes.
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state ^= b as u64;
            self.state = self.state.wrapping_mul(FNV1A64_PRIME);
        }
    }

    /// Consume an i64 (little-endian).
    pub fn write_i64(&mut self, v: i64) {
        self.write(&v.to_le_bytes());
    }

    /// Consume a u64 (little-endian).
    pub fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }

    /// Finish + return the 64-bit digest.
    pub fn finish(self) -> u64 {
        self.state
    }

    /// Render the 64-bit digest as a 16-character lowercase hex
    /// string. Convenient for filesystem-safe cache keys.
    pub fn finish_hex(self) -> String {
        format!("{:016x}", self.state)
    }
}

impl Default for Fnv1a64 {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot 64-bit FNV-1a hash of a byte slice.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = Fnv1a64::new();
    h.write(bytes);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known FNV-1a 64-bit test vectors from the reference site.
    #[test]
    fn known_vectors() {
        // Empty string → offset basis.
        assert_eq!(hash_bytes(b""), FNV1A64_OFFSET);
        // "a" → 0xaf63dc4c8601ec8c per the FNV reference.
        assert_eq!(hash_bytes(b"a"), 0xaf63dc4c8601ec8c);
        // "foobar" → 0x85944171f73967e8.
        assert_eq!(hash_bytes(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn writer_is_streaming() {
        let mut a = Fnv1a64::new();
        a.write(b"foo");
        a.write(b"bar");
        let one_shot = hash_bytes(b"foobar");
        assert_eq!(a.finish(), one_shot);
    }

    #[test]
    fn hex_is_16_chars_lowercase() {
        let h = Fnv1a64::new().finish_hex();
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_lowercase())));
    }
}
