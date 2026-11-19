//! Vance hosted build/review service (spec §10 Phase 3, §9). The two pure,
//! testable pieces of the hosted tier:
//!
//! - **review**: the automated gate every publish must pass — the artifact loads
//!   as a real Vance effect, is within the size budget, and (for wasm) declares
//!   no capabilities the sandbox can't grant.
//! - **sign**: an Ed25519 signature over the approved artifact, so the store can
//!   attest it (spec §9 auto-signing).
//!
//! The third piece — compiling arbitrary SOURCE → wasm — is orchestrated by the
//! CLI's `build` command but MUST run in an isolated builder (see `main.rs`);
//! that sandbox is deployment infrastructure, not something this crate provides.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

/// Default size budget for an effect artifact (16 MiB).
pub const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Wasm,
    Shader,
}

/// The outcome of the automated review gate.
pub struct Verdict {
    pub pass: bool,
    /// Blocking reasons (empty ⇒ pass).
    pub reasons: Vec<String>,
    /// Non-blocking notes (e.g. requested capabilities to surface to buyers).
    pub notes: Vec<String>,
    pub sha256: String,
    pub id: Option<String>,
    pub name: Option<String>,
}

/// Run the automated review gate on an effect artifact.
pub fn review(bytes: &[u8], kind: Kind, max_bytes: usize) -> Verdict {
    let mut reasons = Vec::new();
    let mut notes = Vec::new();
    let mut id = None;
    let mut name = None;

    if bytes.len() > max_bytes {
        reasons.push(format!(
            "artifact is {} bytes — over the {max_bytes}-byte budget",
            bytes.len()
        ));
    }

    match kind {
        Kind::Wasm => match vance_plugin_sdk::host::WasmPlugin::load(bytes) {
            Ok(plugin) => {
                let m = plugin.manifest();
                id = Some(m.id.clone());
                name = Some(m.name.clone());
                if m.gpu.as_ref().is_some_and(|g| g.requires_gpu) {
                    // The host runs the CPU process path today; a GPU-required
                    // plugin would be rejected at load — flag it up front.
                    reasons.push("declares requires_gpu, which the host can't run yet".into());
                }
                if let Some(gpu) = &m.gpu {
                    if !gpu.textures.is_empty() {
                        notes.push(format!(
                            "requests {} texture binding(s)",
                            gpu.textures.len()
                        ));
                    }
                }
            }
            Err(e) => reasons.push(format!("not a loadable Vance effect: {e}")),
        },
        Kind::Shader => match vance_plugin_sdk::shaderpkg::parse(bytes) {
            Ok(pkg) => {
                id = Some(pkg.manifest.id.clone());
                name = Some(pkg.manifest.name.clone());
                if let Err(e) = vance_effects::ShaderEffect::validate(&pkg.shader) {
                    reasons.push(format!("shader doesn't compile: {e}"));
                }
            }
            Err(e) => reasons.push(format!("not a valid .vsfx shader package: {e}")),
        },
    }

    Verdict {
        pass: reasons.is_empty(),
        reasons,
        notes,
        sha256: B64.encode(Sha256::digest(bytes)),
        id,
        name,
    }
}

/// Ed25519-sign an approved artifact (base64url of the signature).
pub fn sign(key: &SigningKey, bytes: &[u8]) -> String {
    B64.encode(key.sign(bytes).to_bytes())
}

/// Kind inferred from a file extension (`.wasm` / `.vsfx`).
pub fn kind_from_ext(path: &str) -> Option<Kind> {
    if path.ends_with(".wasm") {
        Some(Kind::Wasm)
    } else if path.ends_with(".vsfx") {
        Some(Kind::Shader)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Verifier, VerifyingKey};
    use vance_plugin_sdk::manifest::{PluginKind, PluginManifest};
    use vance_plugin_sdk::shaderpkg::{self, ShaderPackage};

    fn shader_pkg(body: &str) -> Vec<u8> {
        shaderpkg::build(&ShaderPackage {
            manifest: PluginManifest {
                id: "com.you.fx".into(),
                name: "FX".into(),
                version: "1.0.0".into(),
                author: "You".into(),
                kind: PluginKind::Effect,
                params: vec![],
                gpu: None,
            },
            shader: body.into(),
        })
    }

    #[test]
    fn review_passes_valid_shader_and_fails_bad_ones() {
        let good = shader_pkg("fn effect(c: vec4<f32>, uv: vec2<f32>) -> vec4<f32> { return c; }");
        let v = review(&good, Kind::Shader, DEFAULT_MAX_BYTES);
        assert!(v.pass, "reasons: {:?}", v.reasons);
        assert_eq!(v.id.as_deref(), Some("com.you.fx"));

        // Won't compile → rejected.
        let bad =
            shader_pkg("fn effect(c: vec4<f32>, uv: vec2<f32>) -> vec4<f32> { return nope(c); }");
        assert!(!review(&bad, Kind::Shader, DEFAULT_MAX_BYTES).pass);

        // Over budget → rejected.
        assert!(!review(&good, Kind::Shader, 4).pass);

        // Not a package at all → rejected.
        assert!(!review(b"garbage", Kind::Shader, DEFAULT_MAX_BYTES).pass);
    }

    #[test]
    fn sign_produces_a_verifiable_signature() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let vk: VerifyingKey = key.verifying_key();
        let bytes = b"the-approved-artifact";
        let sig_b64 = sign(&key, bytes);
        let sig = ed25519_dalek::Signature::from_slice(&B64.decode(sig_b64).unwrap()).unwrap();
        assert!(vk.verify(bytes, &sig).is_ok());
        assert!(vk.verify(b"tampered", &sig).is_err());
    }
}
