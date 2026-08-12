-- Vance accounts — M4: paid-effect encryption at rest (spec §7). A paid
-- artifact's object_key points at the CIPHERTEXT; `cek` is the content key the
-- server seals per-device at activation; `plaintext_sha256` is signed into the
-- lease so the client can verify the decrypted bytes before running them.

ALTER TABLE plugin_artifacts ADD COLUMN encrypted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE plugin_artifacts ADD COLUMN cek TEXT;             -- base64url(32-byte content key)
ALTER TABLE plugin_artifacts ADD COLUMN plaintext_sha256 TEXT; -- base64url(sha256(plaintext wasm))
