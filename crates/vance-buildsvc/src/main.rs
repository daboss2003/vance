//! `vance-buildsvc` — the hosted build/review CLI (spec §10 Phase 3, §9).
//!
//!   vance-buildsvc review <artifact> [--wasm|--shader]     # automated gate
//!   vance-buildsvc sign   <artifact> <ed25519-key-file>    # store signature
//!   vance-buildsvc build  <source-dir> [--out <file>]      # source → wasm → review
//!
//! `review`/`sign` are pure + safe. `build` compiles UNTRUSTED source with the
//! wasm toolchain and MUST be run inside an isolated builder (locked-down
//! container: no network, ephemeral FS, CPU/mem/time limits) — this CLI does the
//! orchestration, not the sandboxing.

use ed25519_dalek::SigningKey;
use vance_buildsvc::{kind_from_ext, review, sign, Kind, DEFAULT_MAX_BYTES};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("review") => cmd_review(&args[2..]),
        Some("sign") => cmd_sign(&args[2..]),
        Some("build") => cmd_build(&args[2..]),
        _ => {
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn usage() {
    eprintln!(
        "usage:\n  vance-buildsvc review <artifact> [--wasm|--shader]\n  vance-buildsvc sign <artifact> <key-file>\n  vance-buildsvc build <source-dir> [--out <file>]"
    );
}

fn kind_of(path: &str, args: &[String]) -> Option<Kind> {
    if args.iter().any(|a| a == "--wasm") {
        Some(Kind::Wasm)
    } else if args.iter().any(|a| a == "--shader") {
        Some(Kind::Shader)
    } else {
        kind_from_ext(path)
    }
}

fn print_verdict(v: &vance_buildsvc::Verdict) {
    let json = serde_json::json!({
        "pass": v.pass,
        "id": v.id,
        "name": v.name,
        "sha256": v.sha256,
        "reasons": v.reasons,
        "notes": v.notes,
    });
    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}

fn cmd_review(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        usage();
        return 2;
    };
    let Some(kind) = kind_of(path, args) else {
        eprintln!("can't tell the artifact kind — pass --wasm or --shader");
        return 2;
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return 2;
        }
    };
    let v = review(&bytes, kind, DEFAULT_MAX_BYTES);
    print_verdict(&v);
    if v.pass {
        0
    } else {
        1
    }
}

fn cmd_sign(args: &[String]) -> i32 {
    let (Some(path), Some(keyfile)) = (args.first(), args.get(1)) else {
        usage();
        return 2;
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return 2;
        }
    };
    let key_bytes = match std::fs::read(keyfile) {
        Ok(b) if b.len() == 32 => b,
        Ok(_) => {
            eprintln!("key file must be exactly 32 bytes (raw Ed25519 seed)");
            return 2;
        }
        Err(e) => {
            eprintln!("read {keyfile}: {e}");
            return 2;
        }
    };
    let arr: [u8; 32] = key_bytes.try_into().unwrap();
    let key = SigningKey::from_bytes(&arr);
    println!("{}", sign(&key, &bytes));
    0
}

fn cmd_build(args: &[String]) -> i32 {
    let Some(dir) = args.first() else {
        usage();
        return 2;
    };
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1));

    eprintln!(
        "note: `build` compiles untrusted source — run this ONLY inside an isolated \
         builder (no network, ephemeral FS, CPU/mem/time limits)."
    );

    let manifest = format!("{dir}/Cargo.toml");
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--manifest-path",
            &manifest,
        ])
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("cargo build failed ({s})");
            return 1;
        }
        Err(e) => {
            eprintln!("couldn't run cargo (is the wasm toolchain installed on the builder?): {e}");
            return 1;
        }
    }

    // Find the produced .wasm.
    let release = format!("{dir}/target/wasm32-unknown-unknown/release");
    let wasm = std::fs::read_dir(&release).ok().and_then(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|x| x.to_str()) == Some("wasm"))
    });
    let Some(wasm_path) = wasm else {
        eprintln!("build produced no .wasm in {release}");
        return 1;
    };
    let bytes = std::fs::read(&wasm_path).unwrap_or_default();
    let v = review(&bytes, Kind::Wasm, DEFAULT_MAX_BYTES);
    print_verdict(&v);
    if let Some(out) = out {
        if let Err(e) = std::fs::copy(&wasm_path, out) {
            eprintln!("copy to {out}: {e}");
            return 1;
        }
    }
    if v.pass {
        0
    } else {
        1
    }
}
