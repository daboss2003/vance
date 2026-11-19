//! `.vsfx` shader-effect package (M6, spec §10). A no-code effect: a plaintext
//! manifest (id/name/params) + one WGSL `effect(color, uv)` function. The host
//! reads the manifest here (no GPU) for discovery; the app compiles the shader
//! (via `vance-effects`) at load. JSON so the Effect Studio can round-trip it.

use crate::manifest::PluginManifest;

/// File extension (without the dot) of a shader-effect package.
pub const SHADER_EXTENSION: &str = "vsfx";

/// A shader effect on disk: its manifest + the user's WGSL body.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ShaderPackage {
    /// Plaintext plugin manifest (id, name, params) read at discovery.
    pub manifest: PluginManifest,
    /// The user's WGSL `fn effect(color: vec4<f32>, uv: vec2<f32>) -> vec4<f32>`.
    pub shader: String,
}

/// Parse a package from its JSON bytes; the error is a readable message.
pub fn parse(bytes: &[u8]) -> Result<ShaderPackage, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("shader package: {e}"))
}

/// Serialize a package to pretty-printed JSON bytes (the inverse of
/// [`parse`]).
pub fn build(pkg: &ShaderPackage) -> Vec<u8> {
    serde_json::to_vec_pretty(pkg).expect("shader package serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{PluginKind, PluginManifest};

    #[test]
    fn round_trips() {
        let pkg = ShaderPackage {
            manifest: PluginManifest {
                id: "com.me.glow".into(),
                name: "Glow".into(),
                version: "1.0.0".into(),
                author: "Me".into(),
                kind: PluginKind::Effect,
                params: vec![],
                gpu: None,
            },
            shader: "fn effect(c: vec4<f32>, uv: vec2<f32>) -> vec4<f32> { return c; }".into(),
        };
        let bytes = build(&pkg);
        let back = parse(&bytes).unwrap();
        assert_eq!(back.manifest.id, "com.me.glow");
        assert!(back.shader.contains("effect"));
    }
}
