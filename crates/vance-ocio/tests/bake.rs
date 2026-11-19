//! OCIO bridge tests. The real-conversion tests are gated behind the
//! `ocio` feature (they need the native library); run them with:
//!
//! ```sh
//! cargo test -p vance-ocio --features ocio
//! ```

use vance_ocio::{bake_lut, is_available, OcioError, MAX_LUT_SIZE};

#[test]
fn size_bounds_are_enforced() {
    // Validation happens before any FFI, so this holds in both builds.
    assert!(matches!(
        bake_lut(None, "a", "b", 1),
        Err(OcioError::BadSize(1))
    ));
    assert!(matches!(
        bake_lut(None, "a", "b", MAX_LUT_SIZE + 1),
        Err(OcioError::BadSize(_))
    ));
}

#[cfg(not(feature = "ocio"))]
#[test]
fn unavailable_without_feature() {
    assert!(!is_available());
    assert!(matches!(
        bake_lut(None, "a", "b", 17),
        Err(OcioError::Unavailable)
    ));
    assert!(vance_ocio::version().is_none());
}

#[cfg(feature = "ocio")]
mod with_ocio {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    // A minimal, fully self-contained OCIO v2 config. Colorspace "half"
    // carries a to_scene_reference MatrixTransform of diag(2,2,2,1), so:
    //   half -> ref  = ×2   (apply to_reference)
    //   ref  -> half = ×0.5 (its inverse)
    // That makes every baked value exactly predictable.
    const CONFIG: &str = r#"ocio_profile_version: 2

roles:
  default: ref
  scene_linear: ref

displays:
  disp:
    - !<View> {name: view, colorspace: ref}

active_displays: [disp]
active_views: [view]

colorspaces:
  - !<ColorSpace>
    name: ref
    isdata: false

  - !<ColorSpace>
    name: half
    isdata: false
    to_scene_reference: !<MatrixTransform> {matrix: [2, 0, 0, 0,  0, 2, 0, 0,  0, 0, 2, 0,  0, 0, 0, 1]}
"#;

    fn write_config(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("vance_ocio_test_{tag}.ocio"));
        let mut f = std::fs::File::create(&path).expect("write temp config");
        f.write_all(CONFIG.as_bytes()).expect("write config bytes");
        path
    }

    #[test]
    fn version_links_and_reports() {
        assert!(is_available());
        let v = vance_ocio::version().expect("version present with feature on");
        assert!(!v.is_empty(), "version string should be non-empty: {v:?}");
    }

    #[test]
    fn same_colorspace_bakes_identity() {
        let cfg = write_config("identity");
        let size = 9;
        let lut = bake_lut(Some(&cfg), "ref", "ref", size).expect("bake ref->ref");
        let id = vance_color::Lut3d::identity(size);
        assert_eq!(lut.size, size);
        assert_eq!(lut.values.len(), id.values.len());
        for (a, b) in lut.values.iter().zip(id.values.iter()) {
            assert!(
                (a - b).abs() < 1e-5,
                "ref->ref must be identity: {a} vs {b}"
            );
        }
    }

    #[test]
    fn matrix_transform_halves_values() {
        let cfg = write_config("halve");
        let size = 2;
        // ref -> half applies the inverse of (×2) == ×0.5 to every value.
        let lut = bake_lut(Some(&cfg), "ref", "half", size).expect("bake ref->half");
        let id = vance_color::Lut3d::identity(size);
        for (out, inp) in lut.values.iter().zip(id.values.iter()) {
            assert!(
                (out - inp * 0.5).abs() < 1e-5,
                "expected ×0.5: in={inp} out={out}"
            );
        }
        // The (1,1,1) corner is the last triple → (0.5,0.5,0.5).
        let n = lut.values.len();
        assert!((lut.values[n - 3] - 0.5).abs() < 1e-5);
        assert!((lut.values[n - 2] - 0.5).abs() < 1e-5);
        assert!((lut.values[n - 1] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn unknown_colorspace_errors_cleanly() {
        let cfg = write_config("unknown");
        let err = bake_lut(Some(&cfg), "ref", "no_such_space", 5).unwrap_err();
        assert!(matches!(err, OcioError::Ocio(_)), "got {err:?}");
    }
}
