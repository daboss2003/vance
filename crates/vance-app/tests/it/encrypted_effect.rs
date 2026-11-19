//! M4 end-to-end (client side, no server): a REAL sample effect, encrypted into a
//! `.vef`, is discovered WITHOUT decryption, then decrypted by the entitlement
//! gate and loaded as working wasm — and blocked when the gate holds no key.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use vance_app_lib::engine::EntitlementGate;
use vance_plugin_sdk::container;
use vance_plugin_sdk::discovery::PluginRegistry;
use vance_plugin_sdk::host::WasmPlugin;

fn sample_wasm() -> Option<Vec<u8>> {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../plugins/stylize/target/wasm32-unknown-unknown/release/vance_plugin_stylize.wasm",
    );
    std::fs::read(p).ok()
}

#[test]
fn encrypted_effect_discovers_decrypts_and_loads() {
    let Some(wasm) = sample_wasm() else {
        eprintln!("sample stylize wasm not built — skipping (run `cargo xtask build-plugins`)");
        return;
    };
    let manifest = WasmPlugin::load(&wasm).unwrap().manifest().clone();
    let id = manifest.id.clone();

    // Encrypt like the server: fresh CEK, AES-256-GCM, nonce-prefixed body.
    let cek = [7u8; 32];
    let nonce = [1u8; 12];
    let ct = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&cek))
        .encrypt(Nonce::from_slice(&nonce), wasm.as_slice())
        .unwrap();
    let mut body = nonce.to_vec();
    body.extend_from_slice(&ct);
    let plaintext_sha =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&wasm));

    // Package as a `.vef` (plaintext manifest header + encrypted body) on disk.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(format!("{id}.vef")),
        container::build(&manifest, &body),
    )
    .unwrap();

    // Discovery lists it as encrypted, reading the manifest WITHOUT the key.
    let reg = PluginRegistry::scan(&[dir.path().to_path_buf()]);
    let disc = reg.by_id(&id).expect("discovered the .vef");
    assert!(disc.encrypted, "flagged encrypted");
    assert_eq!(disc.manifest.id, id);

    let on_disk = std::fs::read(&disc.path).unwrap();
    let parsed = container::parse(&on_disk).unwrap();

    // With the key, the gate recovers the exact plaintext, which loads as wasm.
    let gate = EntitlementGate::default();
    gate.set_keys(std::collections::HashMap::from([(
        id.clone(),
        (cek, plaintext_sha),
    )]));
    let recovered = gate.decrypt(&id, &parsed.body).expect("gate decrypts");
    assert_eq!(recovered, wasm, "recovered the exact effect");
    assert_eq!(
        WasmPlugin::load(&recovered).unwrap().manifest().id,
        id,
        "decrypted bytes load as the effect"
    );

    // Without a key, the paid boundary holds: no plaintext ever materialises.
    assert!(
        EntitlementGate::default()
            .decrypt(&id, &parsed.body)
            .is_none(),
        "blocked without a valid key"
    );
}
