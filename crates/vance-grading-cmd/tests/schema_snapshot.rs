//! Snapshot test that fails on ANY drift of the `GradingCommand`
//! schema — the AI assistant's tool-use spec is derived from this enum,
//! and silent schema drift would mean the model's prompt and the engine
//! contract are out of sync. The committed snapshot at
//! `schema/grading_command.schema.json` IS the published API.
//!
//! Workflow: first run writes the snapshot; subsequent runs diff it.
//! Intentional changes: delete the file and re-run.

use std::path::PathBuf;
use vance_grading_cmd::grading_command_schema;

fn snapshot_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("schema");
    p.push("grading_command.schema.json");
    p
}

#[test]
fn schema_matches_committed_snapshot() {
    let actual = serde_json::to_string_pretty(&grading_command_schema()).unwrap() + "\n";
    let path = snapshot_path();
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir schema/");
        std::fs::write(&path, &actual).expect("write initial snapshot");
        eprintln!(
            "(bootstrapped GradingCommand snapshot at {})",
            path.display()
        );
        return;
    }
    let expected = std::fs::read_to_string(&path).expect("read snapshot");
    if actual != expected {
        panic!(
            "GradingCommand JSON Schema drifted from {}.\n\
             - If intentional: delete that file and rerun this test to refresh.\n\
             - Otherwise: revert your schema change (probably a renamed / added / removed variant or field).",
            path.display()
        );
    }
}

#[test]
fn schema_lists_every_op_variant() {
    // Belt + suspenders: even if the snapshot file is missing, this test
    // catches an accidentally-dropped variant by direct inspection.
    let json = serde_json::to_string(&grading_command_schema()).unwrap();
    for op in [
        "add_grade_node",
        "remove_grade_node",
        "reorder_grade_node",
        "set_grade_node_enabled",
        "set_grade_node_label",
        "set_node_mode",
        "set_wheel",
        "set_tone_curve",
        "set_hsl_curve",
        "set_qualifier",
        "set_window",
        "track_window",
        "set_ocio_view_transform",
        "set_project_view_lut",
        "set_project_working_color_space",
    ] {
        assert!(json.contains(op), "schema missing op variant `{op}`");
    }
}
