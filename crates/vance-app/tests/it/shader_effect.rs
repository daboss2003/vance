//! M6 end-to-end: a Studio-authored `.vsfx` shader effect is discovered (no GPU
//! needed to read its manifest), its WGSL compiles, and it processes pixels —
//! the whole no-code chain: package → discover → compile → render.

use vance_effects::ShaderEffect;
use vance_plugin_sdk::discovery::PluginRegistry;
use vance_plugin_sdk::manifest::{ParamSpec, PluginKind, PluginManifest};
use vance_plugin_sdk::shaderpkg::{self, ShaderPackage};

#[test]
fn vsfx_discovers_compiles_and_renders() {
    // What the Effect Studio's "Save" writes.
    let pkg = ShaderPackage {
        manifest: PluginManifest {
            id: "com.you.brighten".into(),
            name: "Brighten".into(),
            version: "1.0.0".into(),
            author: "You".into(),
            kind: PluginKind::Effect,
            params: vec![ParamSpec {
                key: "amount".into(),
                label: "Amount".into(),
                min: 0.0,
                max: 2.0,
                default: 0.5,
            }],
            gpu: None,
        },
        shader: "fn effect(color: vec4<f32>, uv: vec2<f32>) -> vec4<f32> { return vec4<f32>(color.rgb * param(0), color.a); }".into(),
    };

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("com.you.brighten.vsfx"), shaderpkg::build(&pkg)).unwrap();

    // Discovery lists it WITHOUT a GPU, flagged as a shader effect.
    let reg = PluginRegistry::scan(&[dir.path().to_path_buf()]);
    let disc = reg.by_id("com.you.brighten").expect("discovered the .vsfx");
    assert!(disc.shader, "flagged as a shader effect");
    assert_eq!(disc.manifest.params[0].key, "amount");

    // The engine reads the body + compiles + runs it (real GPU, if present).
    let body = shaderpkg::parse(&std::fs::read(&disc.path).unwrap()).unwrap().shader;
    let fx = match ShaderEffect::compile(&body) {
        Ok(fx) => fx,
        Err(e) if e.contains("adapter") => {
            eprintln!("no GPU — skipping render");
            return;
        }
        Err(e) => panic!("compile failed: {e}"),
    };
    let mut rgba = vec![100u8, 100, 100, 255].repeat(4); // 2×2 gray
    fx.process(&mut rgba, 2, 2, &[0.5]).unwrap();
    assert!((rgba[0] as i32 - 50).abs() <= 1, "100·0.5 → ~50, got {}", rgba[0]);
}
