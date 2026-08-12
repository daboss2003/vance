-- Vance accounts — M2: entitlements, installable artifacts, per-device installs
-- (spec §5–§6). WASM effects are platform-independent, so an artifact is one
-- file per sku (platform kept for future native plugins). Timestamps: unix secs.

CREATE TABLE IF NOT EXISTS entitlements (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    sku         TEXT NOT NULL,
    source      TEXT NOT NULL,                 -- dev_grant | sale | grant | free
    state       TEXT NOT NULL DEFAULT 'active',-- active | revoked | expired
    granted_at  INTEGER NOT NULL,
    expires_at  INTEGER,
    UNIQUE (user_id, sku)
);
CREATE INDEX IF NOT EXISTS idx_entitlements_user ON entitlements(user_id);

-- The installable file for a sku. object_key is a local path in dev; an
-- object-store key in prod. dest is the filename to place under the client's
-- plugins dir. requires_entitlement=0 marks a free effect (gate skipped).
CREATE TABLE IF NOT EXISTS plugin_artifacts (
    sku                  TEXT PRIMARY KEY,
    version              TEXT NOT NULL,
    platform             TEXT NOT NULL DEFAULT 'any',
    object_key           TEXT NOT NULL,
    sha256               TEXT NOT NULL,          -- base64url(sha256(bytes)) — the pin
    dest                 TEXT NOT NULL,
    requires_entitlement INTEGER NOT NULL DEFAULT 1,
    display_name         TEXT
);

-- One row per (device, sku) install. The 2-device cap for paid skus is enforced
-- over the active (uninstalled_at IS NULL) rows, oldest auto-dropped (spec §5).
CREATE TABLE IF NOT EXISTS device_installs (
    id                TEXT PRIMARY KEY,
    user_id           TEXT NOT NULL,
    session_id        TEXT NOT NULL,
    sku               TEXT NOT NULL,
    installed_version TEXT,
    installed_at      INTEGER NOT NULL,
    last_seen_at      INTEGER NOT NULL,
    uninstalled_at    INTEGER,
    UNIQUE (session_id, sku)
);
CREATE INDEX IF NOT EXISTS idx_device_installs_user_sku ON device_installs(user_id, sku);
