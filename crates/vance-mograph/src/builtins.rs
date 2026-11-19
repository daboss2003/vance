//! Built-in `.cmgt` templates shipped with Vance so the feature is
//! usable out of the box. Each demonstrates the rig: control binds,
//! keyframe animation, text + rect layers.

use crate::model::{
    Align, Canvas, ColorProp, Content, Control, ControlKind, Interp, Key, Layer, Meta,
    MographTemplate, Prop, RectSpec, TextProp, TextSpec, Transform,
};

/// All built-in templates, in menu order.
pub fn all() -> Vec<MographTemplate> {
    vec![centered_title(), lower_third()]
}

/// Stable `(key, display name)` pairs for the built-in catalog, so the UI
/// can list templates and request one by a key that survives renames.
pub fn catalog() -> Vec<(&'static str, String)> {
    vec![
        ("centered_title", centered_title().meta.name),
        ("lower_third", lower_third().meta.name),
    ]
}

/// Look up a built-in template by its catalog key.
pub fn by_key(key: &str) -> Option<MographTemplate> {
    match key {
        "centered_title" => Some(centered_title()),
        "lower_third" => Some(lower_third()),
        _ => None,
    }
}

fn key(t: f64, v: f64, interp: Interp) -> Key {
    Key { t, v, interp }
}

/// A fade-in opacity lane 0→1 over `[start, start+dur]`.
fn fade_in(start: f64, dur: f64) -> Prop {
    Prop::Keys(vec![
        key(start, 0.0, Interp::Smooth),
        key(start + dur, 1.0, Interp::Linear),
    ])
}

/// Full-frame centered title card on a dark background. Controls:
/// `headline` (text), `color`, `size`.
pub fn centered_title() -> MographTemplate {
    MographTemplate {
        meta: Meta {
            name: "Centered Title".into(),
            author: "Vance".into(),
            version: 1,
            default_duration_s: 4.0,
        },
        canvas: Canvas {
            width: 1920,
            height: 1080,
            background: [0.05, 0.05, 0.07, 1.0],
        },
        controls: vec![
            Control {
                id: "headline".into(),
                label: "Headline".into(),
                kind: ControlKind::Text {
                    default: "Your Title Here".into(),
                },
            },
            Control {
                id: "color".into(),
                label: "Text Color".into(),
                kind: ControlKind::Color {
                    default: [1.0, 1.0, 1.0, 1.0],
                },
            },
            Control {
                id: "size".into(),
                label: "Size".into(),
                kind: ControlKind::Number {
                    default: 96.0,
                    min: 16.0,
                    max: 400.0,
                },
            },
        ],
        layers: vec![Layer {
            id: "headline".into(),
            name: "Headline".into(),
            visible: true,
            content: Content::Text(TextSpec {
                text: TextProp::Bind("headline".into()),
                font: TextProp::Const("Arial".into()),
                size_px: Prop::Bind("size".into()),
                color: ColorProp::Bind("color".into()),
                align: Align::Center,
                line_height: 1.2,
                tracking: 0.0,
                max_width: Some(1600.0),
            }),
            transform: Transform {
                anchor: [Prop::Const(0.5), Prop::Const(0.5)],
                position: [Prop::Const(960.0), Prop::Const(540.0)],
                scale: [Prop::Const(1.0), Prop::Const(1.0)],
                rotation_deg: Prop::Const(0.0),
                opacity: fade_in(0.0, 0.5),
            },
        }],
    }
}

/// Animated lower-third (accent bar + name + role) sliding in from the
/// left. Transparent canvas — author as an overlay. Controls: `title`,
/// `subtitle`, `accent`, `text_color`.
pub fn lower_third() -> MographTemplate {
    let text_color = || ColorProp::Bind("text_color".into());
    MographTemplate {
        meta: Meta {
            name: "Lower Third — Slide".into(),
            author: "Vance".into(),
            version: 1,
            default_duration_s: 6.0,
        },
        canvas: Canvas {
            width: 1920,
            height: 1080,
            background: [0.0, 0.0, 0.0, 0.0],
        },
        controls: vec![
            Control {
                id: "title".into(),
                label: "Name".into(),
                kind: ControlKind::Text {
                    default: "Jane Doe".into(),
                },
            },
            Control {
                id: "subtitle".into(),
                label: "Role".into(),
                kind: ControlKind::Text {
                    default: "Director of Photography".into(),
                },
            },
            Control {
                id: "accent".into(),
                label: "Accent".into(),
                kind: ControlKind::Color {
                    default: [0.95, 0.55, 0.1, 1.0],
                },
            },
            Control {
                id: "text_color".into(),
                label: "Text Color".into(),
                kind: ControlKind::Color {
                    default: [1.0, 1.0, 1.0, 1.0],
                },
            },
        ],
        layers: vec![
            // Accent bar, sliding in from off-left.
            Layer {
                id: "bar".into(),
                name: "Accent Bar".into(),
                visible: true,
                content: Content::Rect(RectSpec {
                    width: Prop::Const(10.0),
                    height: Prop::Const(120.0),
                    radius: Prop::Const(2.0),
                    color: ColorProp::Bind("accent".into()),
                }),
                transform: Transform {
                    anchor: [Prop::Const(0.0), Prop::Const(0.0)],
                    position: [
                        Prop::Keys(vec![
                            key(0.0, -40.0, Interp::Smooth),
                            key(0.4, 120.0, Interp::Linear),
                        ]),
                        Prop::Const(760.0),
                    ],
                    scale: [Prop::Const(1.0), Prop::Const(1.0)],
                    rotation_deg: Prop::Const(0.0),
                    opacity: fade_in(0.0, 0.3),
                },
            },
            // Name.
            Layer {
                id: "title".into(),
                name: "Name".into(),
                visible: true,
                content: Content::Text(TextSpec {
                    text: TextProp::Bind("title".into()),
                    font: TextProp::Const("Arial".into()),
                    size_px: Prop::Const(64.0),
                    color: text_color(),
                    align: Align::Left,
                    line_height: 1.1,
                    tracking: 0.0,
                    max_width: None,
                }),
                transform: Transform {
                    anchor: [Prop::Const(0.0), Prop::Const(0.0)],
                    position: [
                        Prop::Keys(vec![
                            key(0.05, 100.0, Interp::Smooth),
                            key(0.45, 160.0, Interp::Linear),
                        ]),
                        Prop::Const(752.0),
                    ],
                    scale: [Prop::Const(1.0), Prop::Const(1.0)],
                    rotation_deg: Prop::Const(0.0),
                    opacity: fade_in(0.1, 0.35),
                },
            },
            // Role.
            Layer {
                id: "subtitle".into(),
                name: "Role".into(),
                visible: true,
                content: Content::Text(TextSpec {
                    text: TextProp::Bind("subtitle".into()),
                    font: TextProp::Const("Arial".into()),
                    size_px: Prop::Const(34.0),
                    color: text_color(),
                    align: Align::Left,
                    line_height: 1.1,
                    tracking: 0.5,
                    max_width: None,
                }),
                transform: Transform {
                    anchor: [Prop::Const(0.0), Prop::Const(0.0)],
                    position: [Prop::Const(160.0), Prop::Const(836.0)],
                    scale: [Prop::Const(1.0), Prop::Const(1.0)],
                    rotation_deg: Prop::Const(0.0),
                    opacity: fade_in(0.2, 0.4),
                },
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{ControlValue, MographInstance};
    use crate::{Content, MographEngine};

    #[test]
    fn builtins_have_bound_controls_present() {
        // Every Bind in a built-in rig must reference a real control.
        for tpl in all() {
            let ids: std::collections::HashSet<&str> =
                tpl.controls.iter().map(|c| c.id.as_str()).collect();
            for layer in &tpl.layers {
                let mut binds: Vec<&str> = Vec::new();
                collect_binds(&layer.content, &mut binds);
                for b in binds {
                    assert!(
                        ids.contains(b),
                        "{}: bind '{b}' has no control",
                        tpl.meta.name
                    );
                }
            }
        }
    }

    fn collect_binds<'a>(content: &'a Content, out: &mut Vec<&'a str>) {
        use crate::model::{ColorProp, Prop, TextProp};
        let mut prop = |p: &'a Prop| {
            if let Prop::Bind(id) | Prop::PointX(id) | Prop::PointY(id) = p {
                out.push(id);
            }
        };
        match content {
            Content::Rect(r) => {
                prop(&r.width);
                prop(&r.height);
                prop(&r.radius);
                if let ColorProp::Bind(id) = &r.color {
                    out.push(id);
                }
            }
            Content::Text(t) => {
                prop(&t.size_px);
                if let TextProp::Bind(id) = &t.text {
                    out.push(id);
                }
                if let ColorProp::Bind(id) = &t.color {
                    out.push(id);
                }
            }
        }
    }

    #[test]
    fn point_binding_positions_a_layer() {
        // Build a 1-layer template whose rect is positioned via a Point
        // control; moving the point moves the opaque pixels.
        use crate::model::*;
        let tpl = MographTemplate {
            meta: Meta {
                name: "pt".into(),
                author: String::new(),
                version: 1,
                default_duration_s: 1.0,
            },
            canvas: Canvas {
                width: 100,
                height: 100,
                background: [0.0, 0.0, 0.0, 0.0],
            },
            controls: vec![Control {
                id: "p".into(),
                label: "P".into(),
                kind: ControlKind::Point {
                    default: [0.1, 0.1],
                },
            }],
            layers: vec![Layer {
                id: "box".into(),
                name: "box".into(),
                visible: true,
                content: Content::Rect(RectSpec {
                    width: Prop::Const(10.0),
                    height: Prop::Const(10.0),
                    radius: Prop::Const(0.0),
                    color: ColorProp::Const([1.0, 1.0, 1.0, 1.0]),
                }),
                transform: Transform {
                    anchor: [Prop::Const(0.5), Prop::Const(0.5)],
                    position: [Prop::PointX("p".into()), Prop::PointY("p".into())],
                    scale: [Prop::Const(1.0), Prop::Const(1.0)],
                    rotation_deg: Prop::Const(0.0),
                    opacity: Prop::Const(1.0),
                },
            }],
        };
        let eng = MographEngine::new();
        let mut inst = MographInstance::new(tpl);
        // Default point [0.1,0.1] → box centered near (10,10).
        inst.set("p", ControlValue::Point([0.1, 0.1]));
        let img = eng.render(&inst, 0.0, 30.0, 1.0);
        let alpha_at = |x: u32, y: u32| img.pixels[((y * img.width + x) * 4 + 3) as usize];
        assert!(alpha_at(10, 10) > 200, "box should be near (10,10)");
        assert_eq!(alpha_at(80, 80), 0, "far corner empty");

        // Move the point to (0.8, 0.8) → box near (80,80).
        inst.set("p", ControlValue::Point([0.8, 0.8]));
        let img2 = eng.render(&inst, 0.0, 30.0, 1.0);
        let alpha2 = |x: u32, y: u32| img2.pixels[((y * img2.width + x) * 4 + 3) as usize];
        assert!(alpha2(80, 80) > 200, "box should have moved to (80,80)");
        assert_eq!(alpha2(10, 10), 0, "old spot now empty");
    }
}
