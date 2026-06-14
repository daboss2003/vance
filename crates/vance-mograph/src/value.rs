//! Per-instance control values + the instance that pairs a template
//! with the editor's overrides.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::{Control, ControlKind, MographTemplate, Rgba};

/// A concrete value for one [`Control`], stored per instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ControlValue {
    /// Text body.
    Text(String),
    /// Number.
    Number(f64),
    /// RGBA color.
    Color(Rgba),
    /// Selected `Choice` index.
    Choice(usize),
    /// Boolean.
    Bool(bool),
    /// Font family.
    Font(String),
    /// Normalized point `[0,1]²`.
    Point([f64; 2]),
}

impl ControlValue {
    /// The default value for a control kind.
    pub fn default_for(kind: &ControlKind) -> ControlValue {
        match kind {
            ControlKind::Text { default } => ControlValue::Text(default.clone()),
            ControlKind::Number { default, .. } => ControlValue::Number(*default),
            ControlKind::Color { default } => ControlValue::Color(*default),
            ControlKind::Choice { default, .. } => ControlValue::Choice(*default),
            ControlKind::Bool { default } => ControlValue::Bool(*default),
            ControlKind::Font { default } => ControlValue::Font(default.clone()),
            ControlKind::Point { default } => ControlValue::Point(*default),
        }
    }
}

/// A placed template: the template plus the editor's per-control
/// overrides. Controls without an override fall back to their default.
///
/// The template is embedded inline so the instance round-trips
/// losslessly in the project bundle without a separate template library
/// (a shared library + reference is a later optimization).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MographInstance {
    /// The template rig.
    pub template: MographTemplate,
    /// Control id → value overrides.
    #[serde(default)]
    pub values: BTreeMap<String, ControlValue>,
}

impl MographInstance {
    /// Place `template` with every control at its default.
    pub fn new(template: MographTemplate) -> Self {
        MographInstance {
            template,
            values: BTreeMap::new(),
        }
    }

    /// Resolved value for `control_id`: the override if present and
    /// type-correct, else the control's default. `None` if no such
    /// control exists.
    pub fn value(&self, control_id: &str) -> Option<ControlValue> {
        let ctrl = self.control(control_id)?;
        if let Some(v) = self.values.get(control_id) {
            if value_matches(v, &ctrl.kind) {
                return Some(v.clone());
            }
        }
        Some(ControlValue::default_for(&ctrl.kind))
    }

    /// Look up a control definition by id.
    pub fn control(&self, control_id: &str) -> Option<&Control> {
        self.template.controls.iter().find(|c| c.id == control_id)
    }

    /// Set/replace a control override. Ignores unknown ids and
    /// type-mismatched values so a malformed UI call can't corrupt the
    /// instance.
    pub fn set(&mut self, control_id: &str, value: ControlValue) -> bool {
        match self.control(control_id) {
            Some(c) if value_matches(&value, &c.kind) => {
                self.values.insert(control_id.to_string(), value);
                true
            }
            _ => false,
        }
    }
}

/// Whether a value's variant matches a control's kind.
fn value_matches(v: &ControlValue, kind: &ControlKind) -> bool {
    matches!(
        (v, kind),
        (ControlValue::Text(_), ControlKind::Text { .. })
            | (ControlValue::Number(_), ControlKind::Number { .. })
            | (ControlValue::Color(_), ControlKind::Color { .. })
            | (ControlValue::Choice(_), ControlKind::Choice { .. })
            | (ControlValue::Bool(_), ControlKind::Bool { .. })
            | (ControlValue::Font(_), ControlKind::Font { .. })
            | (ControlValue::Point(_), ControlKind::Point { .. })
    )
}
