-- Vance accounts — M3: the catalog/store listing (spec §9). Logically
-- store.vance; co-located here to reuse the DB + artifact/download machinery.
-- A listing references its installable file by sku (plugin_artifacts.sku).
-- JSON-text columns (categories/tags/capabilities) are parsed to arrays in
-- responses. price_cents=0 = free (M3 browses/installs free effects).

CREATE TABLE IF NOT EXISTS catalog_effects (
    sku          TEXT PRIMARY KEY,              -- == manifest id == plugin_artifacts.sku
    publisher    TEXT NOT NULL,
    display_name TEXT NOT NULL,
    description  TEXT,
    kind         TEXT NOT NULL DEFAULT 'effect',-- effect | transition
    categories   TEXT NOT NULL DEFAULT '[]',
    tags         TEXT NOT NULL DEFAULT '[]',
    capabilities TEXT NOT NULL DEFAULT '[]',    -- requested caps shown on install (spec §9)
    price_cents  INTEGER NOT NULL DEFAULT 0,    -- 0 = free
    icon         TEXT,
    downloads    INTEGER NOT NULL DEFAULT 0,
    listed       INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_catalog_listed ON catalog_effects(listed);

-- Reviews are gated to users who own/installed the effect (spec §9).
CREATE TABLE IF NOT EXISTS catalog_reviews (
    id         TEXT PRIMARY KEY,
    sku        TEXT NOT NULL,
    user_id    TEXT NOT NULL,
    stars      INTEGER NOT NULL,                -- 1..5
    body       TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (sku, user_id)
);
CREATE INDEX IF NOT EXISTS idx_catalog_reviews_sku ON catalog_reviews(sku);
