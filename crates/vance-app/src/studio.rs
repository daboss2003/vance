//! M6c — the Effect Studio backend (spec §10). A creator writes one WGSL
//! `effect(color, uv)` function + declares sliders; these commands compile it
//! for a live preview and save it as a shareable `.vsfx` shader effect (which
//! discovery then lists like any other add-on effect).

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use vance_effects::ShaderEffect;
use vance_plugin_sdk::manifest::{ParamSpec, PluginKind, PluginManifest};
use vance_plugin_sdk::shaderpkg::{self, ShaderPackage};

use crate::engine::EngineHandle;

#[derive(Deserialize, Clone)]
pub struct StudioParam {
    key: String,
    label: String,
    min: f32,
    max: f32,
    default: f32,
}

/// A colourful gradient + radial structure, so an effect's look is obvious.
fn sample_image(w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let maxr = (cx * cx + cy * cy).sqrt();
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let (dxv, dyv) = (x as f32 - cx, y as f32 - cy);
            let radial = 1.0 - (dxv * dxv + dyv * dyv).sqrt() / maxr; // 1 centre → 0 corner
            out[i] = (fx * 255.0) as u8;
            out[i + 1] = ((1.0 - fy) * 255.0) as u8;
            out[i + 2] = (radial.clamp(0.0, 1.0) * 255.0) as u8;
            out[i + 3] = 255;
        }
    }
    out
}

/// Compile + run the shader on a sample image; return `{width,height,rgba}`
/// (base64 RGBA) for a canvas, or a compile error the Studio shows inline.
#[tauri::command]
pub async fn shader_preview(body: String, params: Vec<f32>) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || {
        let fx = ShaderEffect::compile(&body)?;
        let (w, h) = (320u32, 180u32);
        let mut rgba = sample_image(w, h);
        fx.process(&mut rgba, w, h, &params)?;
        Ok(json!({ "width": w, "height": h, "rgba": B64.encode(&rgba) }))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn safe_sku(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err("id must be like com.you.myeffect (letters, numbers, . - _)".into())
    }
}

/// Validate + save the shader as a `.vsfx` in the add-ons folder, then rescan so
/// it's immediately usable (and shareable/sellable).
#[tauri::command]
pub async fn shader_save(
    engine: tauri::State<'_, EngineHandle>,
    id: String,
    name: String,
    author: String,
    body: String,
    params: Vec<StudioParam>,
) -> Result<Value, String> {
    safe_sku(&id)?;
    if name.trim().is_empty() {
        return Err("give your effect a name".into());
    }
    // Validate the shader compiles (naga; no GPU needed) before writing.
    ShaderEffect::validate(&body)?;

    let manifest = PluginManifest {
        id: id.clone(),
        name,
        version: "1.0.0".into(),
        author: if author.trim().is_empty() {
            "You".into()
        } else {
            author
        },
        kind: PluginKind::Effect,
        params: params
            .iter()
            .map(|p| ParamSpec {
                key: p.key.clone(),
                label: p.label.clone(),
                min: p.min,
                max: p.max,
                default: p.default,
            })
            .collect(),
        gpu: None,
    };
    let pkg = ShaderPackage {
        manifest,
        shader: body,
    };
    let bytes = shaderpkg::build(&pkg);

    let dir = vance_plugin_sdk::discovery::user_plugins_dir()
        .ok_or_else(|| "couldn't locate the add-ons folder".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{id}.vsfx"));
    let tmp = path.with_extension("vsfx.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;

    let found = crate::accounts::rescan_after_export(&engine).await?;
    Ok(json!({ "id": id, "installed": found }))
}

/// A starter template so the editor isn't a blank page.
#[tauri::command]
pub fn shader_template() -> Value {
    json!({
        "id": "com.you.myeffect",
        "name": "My Effect",
        "body": "// param(0) = amount. Try changing the maths!\nfn effect(color: vec4<f32>, uv: vec2<f32>) -> vec4<f32> {\n    let amount = param(0);\n    // Boost saturation by `amount`.\n    let gray = dot(color.rgb, vec3<f32>(0.299, 0.587, 0.114));\n    let rgb = mix(vec3<f32>(gray), color.rgb, amount);\n    return vec4<f32>(rgb, color.a);\n}",
        "params": [{ "key": "amount", "label": "Amount", "min": 0.0, "max": 2.0, "default": 1.4 }]
    })
}
