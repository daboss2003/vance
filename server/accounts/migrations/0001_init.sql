-- Vance accounts — M1 schema (accounts, OAuth identities, sessions/devices,
-- pending auth for the loopback-PKCE handshake, step-up challenges).
-- SQLite for local dev; SQL kept portable toward the Postgres prod target
-- (spec docs/marketplace-spec.md §12). Timestamps are unix seconds (INTEGER).

CREATE TABLE IF NOT EXISTS users (
    id              TEXT PRIMARY KEY,
    -- NOT unique: account identity is (provider, provider_subject), and we never
    -- auto-merge two providers by email (spec §4.1), so the same email can back
    -- more than one account. May also be null (Apple private relay / no email).
    email           TEXT,
    email_verified  INTEGER NOT NULL DEFAULT 0,
    password_hash   TEXT,                       -- null until the user sets a password (spec §4.2)
    password_set_at INTEGER,
    ent_ver         INTEGER NOT NULL DEFAULT 0, -- bumped on any entitlement change (spec §5/§6)
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_users_email ON users(email);

-- Account keying is ALWAYS (provider, provider_subject), never email (spec §4.1).
CREATE TABLE IF NOT EXISTS oauth_identities (
    id                TEXT PRIMARY KEY,
    user_id           TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider          TEXT NOT NULL,            -- dev | google | apple | github | meta
    provider_subject  TEXT NOT NULL,
    email_at_provider TEXT,
    linked_at         INTEGER NOT NULL,
    UNIQUE (provider, provider_subject)
);

-- One row per (user, device). Re-auth on the same machine reuses the row, so a
-- machine is one stable device (spec §5). device_id is a client-generated UUID,
-- NOT a hardware fingerprint.
CREATE TABLE IF NOT EXISTS sessions (
    id                 TEXT PRIMARY KEY,        -- session id (sid claim)
    user_id            TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id          TEXT NOT NULL,
    device_name        TEXT NOT NULL,
    platform           TEXT NOT NULL,
    app_version        TEXT,
    refresh_hash       TEXT,                    -- sha256(current refresh token); plaintext never stored
    refresh_generation INTEGER NOT NULL DEFAULT 0,
    prev_refresh_hash  TEXT,                    -- immediately-previous, accepted within the overlap window
    prev_rotated_at    INTEGER,                 -- when the current rotation happened (overlap clock)
    created_at         INTEGER NOT NULL,
    last_seen_at       INTEGER NOT NULL,
    ip_last            TEXT,
    revoked_at         INTEGER,
    revoked_reason     TEXT,
    UNIQUE (user_id, device_id)
);

CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_refresh ON sessions(refresh_hash);
CREATE INDEX IF NOT EXISTS idx_sessions_prev_refresh ON sessions(prev_refresh_hash);

-- Short-lived state for the loopback-PKCE handshake. `state` keys phase 1
-- (/auth/start -> provider); `login_code` keys phase 3 (/auth/token) and is
-- minted at the provider callback. Rows expire quickly and are single-use.
CREATE TABLE IF NOT EXISTS pending_auth (
    id             TEXT PRIMARY KEY,
    state          TEXT UNIQUE NOT NULL,
    login_code     TEXT UNIQUE,
    code_challenge TEXT NOT NULL,               -- PKCE S256 challenge from the client
    redirect_uri   TEXT NOT NULL,               -- the client's 127.0.0.1 loopback
    provider       TEXT NOT NULL,
    device_id      TEXT NOT NULL,
    device_name    TEXT NOT NULL,
    platform       TEXT NOT NULL,
    app_version    TEXT,
    user_id        TEXT,                         -- set at callback
    session_id     TEXT,                         -- set at callback
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    consumed_at    INTEGER
);

-- Step-up ("sudo") challenges for sensitive actions (spec §4.4): purchase,
-- remove-device, change-password. A single-use, action+target-scoped token.
CREATE TABLE IF NOT EXISTS step_up_challenges (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id  TEXT NOT NULL,
    action      TEXT NOT NULL,                   -- remove_device | change_password | purchase
    target_ref  TEXT,
    token_hash  TEXT NOT NULL,                   -- sha256(step-up token)
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    consumed_at INTEGER
);
