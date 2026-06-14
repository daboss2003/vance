use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum KeyframeInterp { #[default] Linear, Bezier, Hold }

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Keyframe {
    pub time_seconds: f64,
    pub value: f64,
    #[serde(default)]
    pub interp: KeyframeInterp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffectValue {
    Number(f64),
    Bool(bool),
    String(String),
    Keyframes(Vec<Keyframe>),
    Expr { expr: String },
}

fn roundtrip(label: &str, v: &EffectValue) {
    // Replicate the project's exact PrettyConfig: struct_names(true).
    let pretty = ron::ser::PrettyConfig::new().struct_names(true).indentor("  ".to_string());
    let body = ron::ser::to_string_pretty(v, pretty).unwrap();
    print!("[{label}] serialized:\n{body}\n");
    match ron::de::from_str::<EffectValue>(&body) {
        Ok(back) => {
            if &back == v { println!("  -> ROUNDTRIP OK\n"); }
            else { println!("  -> ROUNDTRIP MISMATCH: got {back:?}\n"); }
        }
        Err(e) => println!("  -> DESERIALIZE ERROR: {e}\n"),
    }
}

fn main() {
    roundtrip("Number", &EffectValue::Number(4.5));
    roundtrip("Bool", &EffectValue::Bool(true));
    roundtrip("String", &EffectValue::String("hi".into()));
    roundtrip("Keyframes", &EffectValue::Keyframes(vec![
        Keyframe { time_seconds: 0.0, value: 0.0, interp: KeyframeInterp::Linear },
        Keyframe { time_seconds: 1.0, value: 1.0, interp: KeyframeInterp::Bezier },
    ]));
    roundtrip("Expr", &EffectValue::Expr { expr: "value + 30.0 * noise(time * 2.0)".into() });

    // Also test inside a map (how params are actually stored), default PrettyConfig.
    use std::collections::BTreeMap;
    let mut m: BTreeMap<String, EffectValue> = BTreeMap::new();
    m.insert("opacity".into(), EffectValue::Expr { expr: "value".into() });
    m.insert("scale".into(), EffectValue::Number(2.0));
    let pretty = ron::ser::PrettyConfig::new().struct_names(true).indentor("  ".to_string());
    let body = ron::ser::to_string_pretty(&m, pretty).unwrap();
    println!("[map] serialized:\n{body}");
    match ron::de::from_str::<BTreeMap<String, EffectValue>>(&body) {
        Ok(back) => println!("  -> map roundtrip {}\n", if back == m {"OK"} else {"MISMATCH"}),
        Err(e) => println!("  -> map DESERIALIZE ERROR: {e}\n"),
    }
}
