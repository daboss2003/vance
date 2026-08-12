//! `.vef` container — a plaintext manifest header + an encrypted effect body,
//! so plugin discovery can list a paid effect **without decrypting it**, and the
//! host decrypts the body only at load (spec §7). Layout (little-endian):
//!
//! ```text
//! "VEF1" (4) | manifest_len: u32 (4) | manifest_json (manifest_len) | body (rest)
//! ```
//!
//! The `body` is the AEAD ciphertext of the plaintext `.wasm` (nonce-prefixed);
//! this crate treats it as opaque bytes — the actual encryption/decryption + key
//! handling live in the app (`vance-app`), which never writes plaintext to disk.

use crate::manifest::PluginManifest;

pub const VEF_MAGIC: &[u8; 4] = b"VEF1";
pub const VEF_EXTENSION: &str = "vef";

/// Error parsing a `.vef` container.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    /// Missing/incorrect magic — not a `.vef` file.
    #[error("not a .vef container")]
    BadMagic,
    /// The declared manifest length runs past the end of the buffer.
    #[error("truncated .vef container")]
    Truncated,
    /// The manifest header didn't parse as JSON.
    #[error("manifest json: {0}")]
    Manifest(String),
}

/// A parsed container: its manifest header + the still-encrypted body.
pub struct VefContainer {
    /// Plaintext manifest (safe to read without the key).
    pub manifest: PluginManifest,
    /// Opaque encrypted body (nonce-prefixed AEAD ciphertext of the `.wasm`).
    pub body: Vec<u8>,
}

/// Build a `.vef` from a manifest header + an (already-encrypted) body.
pub fn build(manifest: &PluginManifest, body: &[u8]) -> Vec<u8> {
    let json = serde_json::to_vec(manifest).expect("manifest serialize");
    let mut out = Vec::with_capacity(8 + json.len() + body.len());
    out.extend_from_slice(VEF_MAGIC);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(body);
    out
}

fn manifest_span(bytes: &[u8]) -> Result<(usize, usize), ContainerError> {
    if bytes.len() < 8 || &bytes[0..4] != VEF_MAGIC {
        return Err(ContainerError::BadMagic);
    }
    let len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let end = 8usize.checked_add(len).ok_or(ContainerError::Truncated)?;
    if bytes.len() < end {
        return Err(ContainerError::Truncated);
    }
    Ok((8, end))
}

/// Full parse: manifest + body.
pub fn parse(bytes: &[u8]) -> Result<VefContainer, ContainerError> {
    let (start, end) = manifest_span(bytes)?;
    let manifest = serde_json::from_slice(&bytes[start..end])
        .map_err(|e| ContainerError::Manifest(e.to_string()))?;
    Ok(VefContainer {
        manifest,
        body: bytes[end..].to_vec(),
    })
}

/// Read just the manifest (for discovery — no body copy).
pub fn parse_manifest(bytes: &[u8]) -> Result<PluginManifest, ContainerError> {
    let (start, end) = manifest_span(bytes)?;
    serde_json::from_slice(&bytes[start..end]).map_err(|e| ContainerError::Manifest(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{PluginKind, PluginManifest};

    fn sample() -> PluginManifest {
        PluginManifest {
            id: "com.test.glow".into(),
            name: "Glow".into(),
            version: "1.0.0".into(),
            author: "Vance".into(),
            kind: PluginKind::Effect,
            params: vec![],
            gpu: None,
        }
    }

    #[test]
    fn round_trips_header_and_body() {
        let body = b"\x01\x02\x03nonce+ciphertext".to_vec();
        let vef = build(&sample(), &body);
        assert_eq!(&vef[0..4], VEF_MAGIC);
        let parsed = parse(&vef).unwrap();
        assert_eq!(parsed.manifest.id, "com.test.glow");
        assert_eq!(parsed.body, body);
        assert_eq!(parse_manifest(&vef).unwrap().name, "Glow");
    }

    #[test]
    fn rejects_non_vef_and_truncated() {
        assert!(matches!(parse(b"\0asm not vef"), Err(ContainerError::BadMagic)));
        let mut vef = build(&sample(), b"x");
        vef.truncate(6); // cut inside the header
        assert!(parse(&vef).is_err());
    }
}
