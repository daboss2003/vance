//! M6 Phase 2 — the node-graph → WGSL compiler (spec §10). A creator wires a
//! small palette of nodes (Source → transforms → Output); this compiles the
//! graph to the SAME `effect(color, uv)` WGSL body + params list the Effect
//! Studio's code mode produces, so the live-preview + save pipeline is reused.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

// --- palette ---------------------------------------------------------------

struct Param {
    key: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    default: f32,
}
struct NodeKind {
    key: &'static str,
    label: &'static str,
    inputs: &'static [&'static str],
    params: &'static [Param],
}

const KINDS: &[NodeKind] = &[
    NodeKind { key: "input", label: "Source", inputs: &[], params: &[] },
    NodeKind { key: "output", label: "Output", inputs: &["in"], params: &[] },
    NodeKind {
        key: "brightness",
        label: "Brightness",
        inputs: &["in"],
        params: &[Param { key: "amount", label: "Amount", min: 0.0, max: 2.0, default: 1.0 }],
    },
    NodeKind {
        key: "contrast",
        label: "Contrast",
        inputs: &["in"],
        params: &[Param { key: "amount", label: "Amount", min: 0.0, max: 2.0, default: 1.0 }],
    },
    NodeKind {
        key: "saturation",
        label: "Saturation",
        inputs: &["in"],
        params: &[Param { key: "amount", label: "Amount", min: 0.0, max: 2.0, default: 1.0 }],
    },
    NodeKind {
        key: "gamma",
        label: "Gamma",
        inputs: &["in"],
        params: &[Param { key: "gamma", label: "Gamma", min: 0.1, max: 3.0, default: 1.0 }],
    },
    NodeKind { key: "invert", label: "Invert", inputs: &["in"], params: &[] },
    NodeKind {
        key: "tint",
        label: "Tint",
        inputs: &["in"],
        params: &[
            Param { key: "r", label: "Red", min: 0.0, max: 1.0, default: 1.0 },
            Param { key: "g", label: "Green", min: 0.0, max: 1.0, default: 0.8 },
            Param { key: "b", label: "Blue", min: 0.0, max: 1.0, default: 0.6 },
        ],
    },
    NodeKind {
        key: "mix",
        label: "Blend",
        inputs: &["a", "b"],
        params: &[Param { key: "t", label: "Blend", min: 0.0, max: 1.0, default: 0.5 }],
    },
];

fn kind_of(key: &str) -> Result<&'static NodeKind, String> {
    KINDS
        .iter()
        .find(|k| k.key == key)
        .ok_or_else(|| format!("unknown node '{key}'"))
}

/// The WGSL expression for a node, given its input variable names + the global
/// indices of its params.
fn node_expr(kind: &str, ins: &[String], p: &[usize]) -> Result<String, String> {
    let i0 = ins.first().cloned().unwrap_or_default();
    Ok(match kind {
        "brightness" => format!("vec4<f32>({i0}.rgb * param({}), {i0}.a)", p[0]),
        "contrast" => format!(
            "vec4<f32>(({i0}.rgb - vec3<f32>(0.5)) * param({}) + vec3<f32>(0.5), {i0}.a)",
            p[0]
        ),
        "saturation" => format!(
            "vec4<f32>(mix(vec3<f32>(dot({i0}.rgb, vec3<f32>(0.299, 0.587, 0.114))), {i0}.rgb, param({})), {i0}.a)",
            p[0]
        ),
        "gamma" => format!(
            "vec4<f32>(pow(max({i0}.rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / max(param({}), 0.001))), {i0}.a)",
            p[0]
        ),
        "invert" => format!("vec4<f32>(1.0 - {i0}.rgb, {i0}.a)"),
        "tint" => format!(
            "vec4<f32>({i0}.rgb * vec3<f32>(param({}), param({}), param({})), {i0}.a)",
            p[0], p[1], p[2]
        ),
        "mix" => format!("mix({}, {}, param({}))", ins[0], ins[1], p[0]),
        other => return Err(format!("node '{other}' has no expression")),
    })
}

// --- graph -----------------------------------------------------------------

#[derive(Deserialize)]
pub struct NodeParamVal {
    key: String,
    value: f32,
}
#[derive(Deserialize)]
pub struct GraphNode {
    id: String,
    kind: String,
    #[serde(default)]
    params: Vec<NodeParamVal>,
}
#[derive(Deserialize)]
pub struct GraphEdge {
    from: String,
    to: String,
    to_input: String,
}
#[derive(Deserialize)]
pub struct Graph {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
}

#[derive(Serialize, Debug)]
pub struct OutParam {
    key: String,
    label: String,
    min: f32,
    max: f32,
    default: f32,
}

fn visit<'a>(
    id: &'a str,
    by_id: &HashMap<&'a str, &'a GraphNode>,
    inbound: &HashMap<(&'a str, &'a str), &'a str>,
    state: &mut HashMap<&'a str, u8>,
    order: &mut Vec<&'a str>,
) -> Result<(), String> {
    match state.get(id) {
        Some(2) => return Ok(()),
        Some(1) => return Err("the node graph has a loop".into()),
        _ => {}
    }
    state.insert(id, 1);
    let node = by_id.get(id).ok_or("a connection points at a missing node")?;
    let kind = kind_of(&node.kind)?;
    for inp in kind.inputs {
        let src = inbound
            .get(&(id, *inp))
            .ok_or_else(|| format!("the “{inp}” input of a node isn't connected"))?;
        visit(src, by_id, inbound, state, order)?;
    }
    state.insert(id, 2);
    order.push(id);
    Ok(())
}

/// Compile a graph → `(wgsl_effect_body, params)`.
pub fn compile(graph: &Graph) -> Result<(String, Vec<OutParam>), String> {
    let by_id: HashMap<&str, &GraphNode> =
        graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let outputs: Vec<&GraphNode> = graph.nodes.iter().filter(|n| n.kind == "output").collect();
    if outputs.len() != 1 {
        return Err("add exactly one Output node".into());
    }
    let output = outputs[0];

    let mut inbound: HashMap<(&str, &str), &str> = HashMap::new();
    for e in &graph.edges {
        inbound.insert((e.to.as_str(), e.to_input.as_str()), e.from.as_str());
    }

    let mut order: Vec<&str> = Vec::new();
    let mut state: HashMap<&str, u8> = HashMap::new();
    visit(&output.id, &by_id, &inbound, &mut state, &mut order)?;

    let mut var_of: HashMap<&str, String> = HashMap::new();
    let mut params: Vec<OutParam> = Vec::new();
    let mut body = String::new();

    for (i, id) in order.iter().enumerate() {
        let node = by_id[id];
        let kind = kind_of(&node.kind)?;
        match node.kind.as_str() {
            "input" => {
                var_of.insert(id, "color".into());
            }
            "output" => {
                let src = inbound
                    .get(&(*id, "in"))
                    .ok_or("connect something to the Output node")?;
                let v = var_of
                    .get(src)
                    .ok_or("the Output node's input is unresolved")?;
                body.push_str(&format!("    return {v};\n"));
            }
            _ => {
                let in_vars: Vec<String> = kind
                    .inputs
                    .iter()
                    .map(|inp| var_of[inbound[&(*id, *inp)]].clone())
                    .collect();
                let base = params.len();
                for p in kind.params {
                    let value = node
                        .params
                        .iter()
                        .find(|np| np.key == p.key)
                        .map(|np| np.value)
                        .unwrap_or(p.default);
                    params.push(OutParam {
                        key: format!("{}_{}", id, p.key),
                        label: format!("{} · {}", kind.label, p.label),
                        min: p.min,
                        max: p.max,
                        default: value,
                    });
                }
                let pidx: Vec<usize> = (base..params.len()).collect();
                let expr = node_expr(&node.kind, &in_vars, &pidx)?;
                let var = format!("n{i}");
                body.push_str(&format!("    let {var} = {expr};\n"));
                var_of.insert(id, var);
            }
        }
    }

    let full = format!("fn effect(color: vec4<f32>, uv: vec2<f32>) -> vec4<f32> {{\n{body}}}");
    Ok((full, params))
}

// --- commands --------------------------------------------------------------

/// The node palette (so the editor + compiler stay in sync).
#[tauri::command]
pub fn nodegraph_palette() -> Value {
    let kinds: Vec<Value> = KINDS
        .iter()
        .map(|k| {
            json!({
                "key": k.key,
                "label": k.label,
                "inputs": k.inputs,
                "params": k.params.iter().map(|p| json!({
                    "key": p.key, "label": p.label, "min": p.min, "max": p.max, "default": p.default
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "kinds": kinds })
}

/// Compile a graph to `{ body, params }` for the Studio preview/save pipeline.
#[tauri::command]
pub fn nodegraph_compile(graph: Graph) -> Result<Value, String> {
    let (body, params) = compile(&graph)?;
    Ok(json!({ "body": body, "params": params }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, kind: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            kind: kind.into(),
            params: vec![],
        }
    }
    fn edge(from: &str, to: &str, to_input: &str) -> GraphEdge {
        GraphEdge {
            from: from.into(),
            to: to.into(),
            to_input: to_input.into(),
        }
    }

    #[test]
    fn compiles_a_chain_to_valid_wgsl() {
        // Source → Brightness → Output.
        let g = Graph {
            nodes: vec![node("s", "input"), node("b", "brightness"), node("o", "output")],
            edges: vec![edge("s", "b", "in"), edge("b", "o", "in")],
        };
        let (body, params) = compile(&g).unwrap();
        assert_eq!(params.len(), 1);
        assert!(body.contains("param(0)"));
        assert!(body.contains("return"));
        // The generated body must be valid WGSL for the shader executor.
        vance_effects::ShaderEffect::validate(&body).expect("valid WGSL");
    }

    #[test]
    fn errors_without_output_and_on_cycles() {
        let g = Graph { nodes: vec![node("s", "input")], edges: vec![] };
        assert!(compile(&g).is_err());

        // A 2-node cycle b→c→b feeding output.
        let g = Graph {
            nodes: vec![node("b", "brightness"), node("c", "contrast"), node("o", "output")],
            edges: vec![edge("b", "c", "in"), edge("c", "b", "in"), edge("c", "o", "in")],
        };
        assert!(compile(&g).unwrap_err().contains("loop"));
    }
}
